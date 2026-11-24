#include "frame_link.h"
#include <d3d11.h>
#include <dxgi.h>
#include <GL/gl.h>
#include <wrl/client.h>
#include <gdnative_api_struct.gen.h>
#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>

namespace fl = frame_link;
using Microsoft::WRL::ComPtr;
static const godot_gdnative_core_api_struct *core;
static const godot_gdnative_ext_arvr_api_struct *vr;
static bool probe_finish;
static LONGLONG ready_us;

// WGL_NV_DX_interop2 and OpenGL framebuffer functions from the current render context.
struct Interop {
    HANDLE(WINAPI *open)(void *) {};
    BOOL(WINAPI *close)(HANDLE) {};
    HANDLE(WINAPI *reg)(HANDLE, void *, GLuint, GLenum, GLenum) {};
    BOOL(WINAPI *unreg)(HANDLE, HANDLE) {};
    BOOL(WINAPI *lock)(HANDLE, GLint, HANDLE *) {};
    BOOL(WINAPI *unlock)(HANDLE, GLint, HANDLE *) {};
    void(APIENTRY *gen_fbo)(GLsizei, GLuint *){};
    void(APIENTRY *delete_fbo)(GLsizei, const GLuint *){};
    void(APIENTRY *bind)(GLenum, GLuint){};
    void(APIENTRY *attach)(GLenum, GLenum, GLenum, GLuint, GLint){};
    GLenum(APIENTRY *status)(GLenum) {};
    void(APIENTRY *attachment)(GLenum, GLenum, GLenum, GLint *){};
    void(APIENTRY *blit)(GLint, GLint, GLint, GLint, GLint, GLint, GLint, GLint, GLbitfield, GLenum){};
    template <class P> static bool load(P &proc, const char *name) {
        auto p = wglGetProcAddress(name);
        if (!p || reinterpret_cast<uintptr_t>(p) <= 3 || reinterpret_cast<intptr_t>(p) == -1)
            return false;
        proc = reinterpret_cast<P>(p);
        return true;
    }
    bool init() {
        return load(open, "wglDXOpenDeviceNV") && load(close, "wglDXCloseDeviceNV") &&
               load(reg, "wglDXRegisterObjectNV") && load(unreg, "wglDXUnregisterObjectNV") &&
               load(lock, "wglDXLockObjectsNV") && load(unlock, "wglDXUnlockObjectsNV") &&
               load(gen_fbo, "glGenFramebuffers") && load(delete_fbo, "glDeleteFramebuffers") &&
               load(bind, "glBindFramebuffer") && load(attach, "glFramebufferTexture2D") &&
               load(status, "glCheckFramebufferStatus") &&
               load(attachment, "glGetFramebufferAttachmentParameteriv") && load(blit, "glBlitFramebuffer");
    }
};
constexpr GLenum READ_FB = 0x8CA8, DRAW_FB = 0x8CA9, READ_BIND = 0x8CAA, DRAW_BIND = 0x8CA6;
constexpr GLenum COLOR_ATTACHMENT = 0x8CE0, COMPLETE_FB = 0x8CD5;
struct Surface {
    ComPtr<ID3D11Texture2D> texture;
    ComPtr<IDXGIKeyedMutex> key;
    HANDLE share{}, interop{};
    GLuint gl{};
};

class Publisher {
  public:
    fl::Mapping mapping;
    bool enabled{};
    LONG width = 1280, height = 720;
    LONG token{}, mode = fl::direct;
    bool available() const { return fl::requested(mapping.c); }

    void create_mapping() {
        auto handle = CreateFileMappingW(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE, 0,
                                         sizeof(fl::Control), fl::name(GetCurrentProcessId()).c_str());
        if (!handle)
            return;
        if (GetLastError() == ERROR_ALREADY_EXISTS) {
            CloseHandle(handle);
            return;
        }
        if (!mapping.map(handle))
            return;
        published =
            CreateEventW(nullptr, FALSE, FALSE, (fl::name(GetCurrentProcessId()) + L".frame").c_str());
        if (!published)
            return;
        fl::put(mapping.c->pid, GetCurrentProcessId());
        fl::put(mapping.c->abi, fl::version);
        fl::put(mapping.c->signature, fl::magic);
    }
    void size() {
        if (!available()) {
            if (probe_finish) {
                RECT rect{};
                if (GetClientRect(WindowFromDC(wglGetCurrentDC()), &rect) &&
                    fl::dimensions(rect.right, rect.bottom)) {
                    width = rect.right;
                    height = rect.bottom;
                }
            }
            return;
        }
        auto c = mapping.c;
        LONG w = fl::read(c->requested_width), h = fl::read(c->requested_height);
        if (fl::dimensions(w, h)) {
            width = w;
            height = h;
        }
    }
    GLuint before_draw() {
        if (!available())
            return 0;
        auto c = mapping.c;
        const LONG request = fl::read(c->request);
        if (token != request) {
            token = request;
            fl::put(c->ready, 0);
            size();
            const LONG next_mode = fl::read(c->requested_mode);
            if (allocated_w != width || allocated_h != height || !dx) {
                destroy_gpu();
                int error = setup();
                if (error) {
                    destroy_gpu();
                    fail(error);
                    return 0;
                }
            }
            mode = next_mode == fl::copy ? fl::copy : fl::direct;
            fl::put(c->error, 0);
        }
        if (fl::read(c->error) || mode != fl::direct || !take_slot())
            return 0;
        return surfaces[held].gl;
    }
    void commit(godot_rid *target, godot_rect2 *rect) {
        if (probe_finish) {
            glFinish();
            InterlockedExchange64(&ready_us, fl::clock_us());
        }
        const bool live = available() && token == fl::read(mapping.c->request) && !fl::read(mapping.c->error);
        bool valid = live && held >= 0;
        bool preview = true;
        if (valid && mode == fl::direct) {
            // Stock Godot 3 GLES3 Canvas may re-bind its internal FBO in reset_canvas().
            // Accept direct delivery only when the draw FBO actually contains our texture.
            GLint draw = 0, type = 0, color = 0;
            glGetIntegerv(DRAW_BIND, &draw);
            if (draw) {
                gl.attachment(DRAW_FB, COLOR_ATTACHMENT, 0x8CD0, &type);
                if (type == GL_TEXTURE)
                    gl.attachment(DRAW_FB, COLOR_ATTACHMENT, 0x8CD1, &color);
            }
            if (color != static_cast<GLint>(surfaces[held].gl)) {
                mode = fl::copy;
                valid = color > 0 && copy_final(static_cast<GLuint>(color));
                preview = valid;
                if (fl::read(mapping.c->ready) == token)
                    fl::put(mapping.c->active_mode, fl::copy);
            }
        } else if (live && mode == fl::copy && take_slot()) {
            valid = copy_final(static_cast<GLuint>(vr->godot_arvr_get_texid(target)));
            if (!valid)
                fail(-24);
            // Here the preview reads Godot's private texture, so encoding can
            // start as soon as the relay blit finishes, ahead of the preview blit.
            publish(valid);
        }
        // Godot presents the same mono render target locally, including the HUD.
        const auto extent = core->godot_rect2_get_size(rect);
        const auto pos = core->godot_rect2_get_position(rect);
        float width_px = core->godot_vector2_get_x(&extent), height_px = core->godot_vector2_get_y(&extent);
        if (preview && width_px > 0 && height_px > 0) {
            auto preview_rect = *rect;
            if (token) {
                const float scale = std::min(width_px / width, height_px / height);
                const float w = width * scale, h = height * scale;
                core->godot_rect2_new(&preview_rect, core->godot_vector2_get_x(&pos) + (width_px - w) / 2,
                                      core->godot_vector2_get_y(&pos) + (height_px - h) / 2 + h, w, -h);
            }
            vr->godot_arvr_blit(0, target, &preview_rect);
        }
        if (held >= 0)
            publish(valid);
    }

  private:
    void publish(bool valid) {
        auto &s = surfaces[held];
        if (!gl.unlock(dx, 1, &s.interop)) {
            valid = false;
            fail(-25);
        }
        if (valid)
            fl::put(mapping.c->frames[held].sequence, ++sequence);
        if (s.key->ReleaseSync(valid ? 1 : 0) != S_OK) {
            valid = false;
            fail(-26);
        }
        held = -1;
        if (valid && fl::read(mapping.c->ready) != token) {
            auto c = mapping.c;
            fl::put(c->width, width);
            fl::put(c->height, height);
            fl::put(c->active_mode, mode);
            for (unsigned i = 0; i < fl::slots; ++i)
                fl::put(c->frames[i].handle,
                        static_cast<LONG64>(reinterpret_cast<intptr_t>(surfaces[i].share)));
            fl::put(c->ready, token);
        }
        if (valid)
            SetEvent(published);
    }

  public:
    ~Publisher() {
        if (mapping.c)
            fl::put(mapping.c->signature, 0);
        destroy_gpu();
        if (published)
            CloseHandle(published);
    }

  private:
    Interop gl;
    HANDLE published{};
    HGLRC gl_context{};
    ComPtr<ID3D11Device> device;
    HANDLE dx{};
    Surface surfaces[fl::slots]{};
    GLuint src_fbo{}, dst_fbo{};
    LONG allocated_w{}, allocated_h{};
    unsigned cursor{};
    int held = -1;
    LONG64 sequence{};
    void fail(LONG reason) {
        if (!mapping.c)
            return;
        fl::put(mapping.c->error, reason);
        fl::put(mapping.c->ready, token);
    }
    int setup() {
        gl_context = wglGetCurrentContext();
        if (!gl_context || !gl.init())
            return -20;
        ComPtr<IDXGIFactory1> factory;
        if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory))))
            return -21;
        for (UINT n = 0;; ++n) {
            ComPtr<IDXGIAdapter1> adapter;
            if (factory->EnumAdapters1(n, &adapter) != S_OK)
                break;
            DXGI_ADAPTER_DESC1 description{};
            if (FAILED(adapter->GetDesc1(&description)) || description.VendorId != 0x10de)
                continue;
            if (SUCCEEDED(D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                                            D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION,
                                            &device, nullptr, nullptr))) {
                dx = gl.open(device.Get());
                if (dx)
                    break;
                device.Reset();
            }
        }
        if (!dx)
            return -21;
        D3D11_TEXTURE2D_DESC desc{};
        desc.Width = width;
        desc.Height = height;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        desc.SampleDesc.Count = 1;
        desc.Usage = D3D11_USAGE_DEFAULT;
        desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
        desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX;
        for (auto &s : surfaces) {
            ComPtr<IDXGIResource> resource;
            if (FAILED(device->CreateTexture2D(&desc, nullptr, &s.texture)) ||
                FAILED(s.texture.As(&resource)) || FAILED(resource->GetSharedHandle(&s.share)) ||
                FAILED(s.texture.As(&s.key)))
                return -22;
            glGenTextures(1, &s.gl);
            s.interop = gl.reg(dx, s.texture.Get(), s.gl, GL_TEXTURE_2D, 1); // WGL_ACCESS_READ_WRITE_NV
            if (!s.interop)
                return -23;
        }
        gl.gen_fbo(1, &src_fbo);
        gl.gen_fbo(1, &dst_fbo);
        allocated_w = width;
        allocated_h = height;
        return 0;
    }
    bool take_slot() {
        if (held >= 0 || !dx)
            return false;
        for (unsigned pass = 0; pass < 2; ++pass) {
            for (unsigned k = 0; k < fl::slots; ++k) {
                unsigned i = (cursor + k) % fl::slots;
                auto &s = surfaces[i];
                HRESULT hr = s.key->AcquireSync(pass, 0); // reclaim an unread old picture on pass 1
                if (hr == WAIT_TIMEOUT)
                    continue;
                if (hr != S_OK) {
                    fail(-26);
                    return false;
                }
                if (!gl.lock(dx, 1, &s.interop)) {
                    s.key->ReleaseSync(0);
                    fail(-23);
                    return false;
                }
                held = i;
                cursor = (i + 1) % fl::slots;
                return true;
            }
        }
        return false;
    }
    bool copy_final(GLuint source) {
        if (!source)
            return false;
        GLint old_read = 0, old_draw = 0;
        glGetIntegerv(READ_BIND, &old_read);
        glGetIntegerv(DRAW_BIND, &old_draw);
        const auto scissor = glIsEnabled(GL_SCISSOR_TEST);
        gl.bind(READ_FB, src_fbo);
        gl.attach(READ_FB, COLOR_ATTACHMENT, GL_TEXTURE_2D, source, 0);
        gl.bind(DRAW_FB, dst_fbo);
        gl.attach(DRAW_FB, COLOR_ATTACHMENT, GL_TEXTURE_2D, surfaces[held].gl, 0);
        const bool valid = gl.status(READ_FB) == COMPLETE_FB && gl.status(DRAW_FB) == COMPLETE_FB;
        if (valid) {
            glDisable(GL_SCISSOR_TEST);
            gl.blit(0, 0, width, height, 0, 0, width, height, GL_COLOR_BUFFER_BIT, GL_NEAREST);
            if (scissor)
                glEnable(GL_SCISSOR_TEST);
        }
        gl.bind(READ_FB, old_read);
        gl.bind(DRAW_FB, old_draw);
        return valid;
    }
    void destroy_gpu() {
        const bool current = gl_context && wglGetCurrentContext() == gl_context;
        if (held >= 0) {
            auto &s = surfaces[held];
            if (current && dx && s.interop)
                gl.unlock(dx, 1, &s.interop);
            s.key->ReleaseSync(0);
            held = -1;
        }
        if (current) {
            if (src_fbo)
                gl.delete_fbo(1, &src_fbo);
            if (dst_fbo)
                gl.delete_fbo(1, &dst_fbo);
        }
        for (auto &s : surfaces) {
            if (dx && s.interop)
                gl.unreg(dx, s.interop);
            if (current && s.gl)
                glDeleteTextures(1, &s.gl);
            s = Surface{};
        }
        if (dx)
            gl.close(dx);
        device.Reset();
        dx = nullptr;
        src_fbo = dst_fbo = 0;
        allocated_w = allocated_h = 0;
        held = -1;
    }
};

static Publisher *publisher;
static godot_arvr_interface_gdnative implementation{};
static void *construct(godot_object *) { return publisher; }
static void destruct(void *) {}
static godot_string name(const void *) {
    godot_string n;
    core->godot_string_new(&n);
    core->godot_string_parse_utf8(&n, "LanStreamMono");
    return n;
}
static godot_int capabilities(const void *) { return 1; } // ARVR_MONO
static godot_bool no(const void *) { return false; }
static void ignore_bool(void *, godot_bool) {}
static godot_bool initialized(const void *) { return publisher->enabled; }
static godot_bool initialize(void *) {
    publisher->enabled = true;
    return true;
}
static void uninitialize(void *) { publisher->enabled = false; }
static godot_vector2 size(const void *) {
    publisher->size();
    godot_vector2 v;
    core->godot_vector2_new(&v, static_cast<float>(publisher->width), static_cast<float>(publisher->height));
    return v;
}
static godot_transform transform(void *, godot_int, godot_transform *camera) { return *camera; }
static void projection(void *, godot_real *p, godot_int, godot_real aspect, godot_real near_z,
                       godot_real far_z) {
    std::fill(p, p + 16, 0.0f);
    const float f = 1.428148f;
    p[0] = f / aspect;
    p[5] = f;
    p[10] = (far_z + near_z) / (near_z - far_z);
    p[11] = -1;
    p[14] = 2 * far_z * near_z / (near_z - far_z);
}
static void commit(void *, godot_int, godot_rid *rt, godot_rect2 *rect) { publisher->commit(rt, rect); }
static void idle(void *) {}
static godot_int external(void *, godot_int) { return publisher->before_draw(); }
static void notification(void *, godot_int) {}
static godot_int zero(void *) { return 0; }
static godot_int depth(void *, godot_int) { return 0; }

extern "C" __declspec(dllexport) void godot_gdnative_init(godot_gdnative_init_options *options) {
    core = options->api_struct;
    for (unsigned i = 0; i < core->num_extensions; ++i)
        if (core->extensions[i]->type == GDNATIVE_EXT_ARVR)
            vr = reinterpret_cast<const godot_gdnative_ext_arvr_api_struct *>(core->extensions[i]);
    publisher = new Publisher();
    probe_finish = GetEnvironmentVariableW(L"LAN_STREAM_SIM_FINISH", nullptr, 0) > 0;
    if (!vr || options->in_editor)
        return;
    publisher->create_mapping();
    implementation = {{1, 2}, construct,   destruct,   name,         capabilities, no,        ignore_bool,
                      no,     initialized, initialize, uninitialize, size,         transform, projection,
                      commit, idle,        external,   notification, zero,         depth};
    vr->godot_arvr_register_interface(&implementation);
}
extern "C" __declspec(dllexport) godot_variant lan_stream_requested(godot_array *) {
    const LONG request = publisher && publisher->available() ? fl::read(publisher->mapping.c->request) : 0;
    godot_variant v;
    core->godot_variant_new_int(&v, request);
    return v;
}
extern "C" __declspec(dllexport) godot_variant lan_stream_target_size(godot_array *) {
    godot_vector2 size;
    auto c = publisher->mapping.c;
    core->godot_vector2_new(&size, c ? static_cast<float>(fl::read(c->requested_width)) : 0.0f,
                            c ? static_cast<float>(fl::read(c->requested_height)) : 0.0f);
    godot_variant v;
    core->godot_variant_new_vector2(&v, &size);
    return v;
}
// Same QPC domain as the Windows encoder and the benchmark receiver.
extern "C" __declspec(dllexport) godot_variant lan_stream_clock_us(godot_array *) {
    godot_variant v;
    core->godot_variant_new_int(&v, fl::clock_us());
    return v;
}
extern "C" __declspec(dllexport) godot_variant lan_stream_last_ready_us(godot_array *) {
    godot_variant v;
    core->godot_variant_new_int(&v, InterlockedCompareExchange64(&ready_us, 0, 0));
    return v;
}
extern "C" __declspec(dllexport) void godot_gdnative_terminate(godot_gdnative_terminate_options *) {
    delete publisher;
    publisher = nullptr;
    vr = nullptr;
    core = nullptr;
}
