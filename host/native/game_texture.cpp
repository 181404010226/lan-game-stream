#include "../../render-bridge/frame_link.h"
#include <d3d11.h>
#include <dxgi.h>
#include <wrl/client.h>
#include <new>
#include <memory>
using Microsoft::WRL::ComPtr;
namespace fl = frame_link;

struct GameReader {
    fl::Mapping mapping;
    HANDLE mutex{};
    HANDLE frame_event{};
    bool owns_mutex{};
    LONG token{};
    ComPtr<ID3D11Texture2D> textures[fl::slots];
    ComPtr<IDXGIKeyedMutex> keys[fl::slots];
    LONG64 last{};
    int held = -1;
    LONG mode{};
    ~GameReader() {
        if (held >= 0)
            keys[held]->ReleaseSync(0);
        if (token && mapping.c)
            InterlockedCompareExchange(&mapping.c->request, 0, token);
        if (owns_mutex)
            ReleaseMutex(mutex);
        if (mutex)
            CloseHandle(mutex);
        if (frame_event)
            CloseHandle(frame_event);
    }
};

// Returns 1 for a game without an adapter. All other errors describe a real failed negotiation.
extern "C" int lan_game_open(void *raw_device, DWORD pid, DWORD width, DWORD height, int mode, void **out,
                             int *actual_mode) {
    *out = nullptr;
    auto reader = std::unique_ptr<GameReader>(new (std::nothrow) GameReader);
    if (!reader || !fl::dimensions(width, height))
        return -1;
    const auto name = fl::name(pid);
    HANDLE file = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, FALSE, name.c_str());
    if (!file)
        return GetLastError() == ERROR_FILE_NOT_FOUND ? 1 : -2;
    if (!reader->mapping.map(file))
        return -3;
    reader->frame_event = OpenEventW(SYNCHRONIZE, FALSE, (name + L".frame").c_str());
    if (!reader->frame_event)
        return -3;
    auto c = reader->mapping.c;
    if (fl::read(c->signature) != fl::magic || fl::read(c->abi) != fl::version ||
        fl::read(c->pid) != static_cast<LONG>(pid))
        return -4;
    reader->mutex = CreateMutexW(nullptr, FALSE, (name + L".consumer").c_str());
    if (!reader->mutex)
        return -5;
    const DWORD wait = WaitForSingleObject(reader->mutex, 0);
    if (wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED)
        return -6;
    reader->owns_mutex = true;
    fl::put(c->request, 0);
    reader->token = InterlockedIncrement(&c->serial);
    if (!reader->token)
        reader->token = InterlockedIncrement(&c->serial);
    fl::put(c->requested_width, width);
    fl::put(c->requested_height, height);
    fl::put(c->requested_mode, mode);
    fl::put(c->heartbeat, static_cast<LONG64>(GetTickCount64()));
    fl::put(c->request, reader->token);
    const auto end = GetTickCount64() + 1900;
    while (fl::read(c->ready) != reader->token) {
        if (GetTickCount64() >= end)
            return -7;
        Sleep(2);
    }
    if (fl::read(c->error))
        return fl::read(c->error);
    *actual_mode = fl::read(c->active_mode);
    reader->mode = *actual_mode;
    if (fl::read(c->width) != static_cast<LONG>(width) || fl::read(c->height) != static_cast<LONG>(height) ||
        (*actual_mode != fl::direct && *actual_mode != fl::copy))
        return -8;
    auto device = static_cast<ID3D11Device *>(raw_device);
    for (unsigned i = 0; i < fl::slots; i++) {
        const auto handle = reinterpret_cast<HANDLE>(static_cast<intptr_t>(fl::read(c->frames[i].handle)));
        if (FAILED(device->OpenSharedResource(handle, IID_PPV_ARGS(&reader->textures[i]))) ||
            FAILED(reader->textures[i].As(&reader->keys[i])))
            return -9;
        D3D11_TEXTURE2D_DESC desc{};
        reader->textures[i]->GetDesc(&desc);
        if (desc.Width != width || desc.Height != height || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM ||
            desc.MipLevels != 1 || desc.ArraySize != 1 || desc.SampleDesc.Count != 1)
            return -10;
    }
    *out = reader.release();
    return 0;
}
extern "C" void *lan_game_texture(void *opaque, unsigned slot) {
    auto reader = static_cast<GameReader *>(opaque);
    if (slot >= fl::slots)
        return nullptr;
    auto *texture = reader->textures[slot].Get();
    texture->AddRef();
    return texture;
}
extern "C" int lan_game_acquire(void *opaque) {
    auto &r = *static_cast<GameReader *>(opaque);
    auto c = r.mapping.c;
    if (r.held >= 0 || fl::read(c->signature) != fl::magic || fl::read(c->request) != r.token ||
        fl::read(c->ready) != r.token || fl::read(c->error) || fl::read(c->active_mode) != r.mode)
        return -2;
    fl::put(c->heartbeat, static_cast<LONG64>(GetTickCount64()));
    int best = -1;
    auto latest = r.last;
    for (unsigned i = 0; i < fl::slots; ++i) {
        auto seq = fl::read(c->frames[i].sequence);
        if (seq > latest) {
            latest = seq;
            best = i;
        }
    }
    if (best == -1)
        return -1;
    auto hr = r.keys[best]->AcquireSync(1, 0);
    if (hr == WAIT_TIMEOUT)
        return -1;
    if (hr != S_OK)
        return -3;
    r.held = best;
    r.last = fl::read(c->frames[best].sequence);
    return best;
}
extern "C" int lan_game_release(void *opaque) {
    auto &r = *static_cast<GameReader *>(opaque);
    if (r.held < 0)
        return -1;
    if (r.keys[r.held]->ReleaseSync(0) != S_OK)
        return -2;
    r.held = -1;
    return 0;
}
extern "C" void lan_game_close(void *opaque) { delete static_cast<GameReader *>(opaque); }
extern "C" HANDLE lan_game_event(void *opaque) { return static_cast<GameReader *>(opaque)->frame_event; }
extern "C" int64_t lan_frame_clock_us() { return fl::clock_us(); }
