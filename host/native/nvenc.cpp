#include <windows.h>
#include <new>
#include "nvEncodeAPI.h"

struct EncodeSlot {
    NV_ENC_REGISTERED_PTR resource = nullptr;
    NV_ENC_INPUT_PTR mapped = nullptr;
    NV_ENC_OUTPUT_PTR bitstream = nullptr;
    HANDLE event = nullptr;
    bool registered_event = false;
    uint32_t frame = 0;
};

using ConsumeBitstream = void (*)(void*, const void*, uint32_t);

// One thread owns all D3D11 and NVENC calls. The GPU may still own earlier slots.
struct LanNvenc {
    NV_ENCODE_API_FUNCTION_LIST api{};
    void* encoder = nullptr;
    static constexpr unsigned capacity = 4; // no B-frames
    EncodeSlot slots[capacity]{};
    unsigned head = 0;
    unsigned pending = 0;
    bool asynchronous = false;
    bool submitted = false;
    HANDLE timer = nullptr;
    HANDLE eos = nullptr;
    bool registered_eos = false;
    unsigned width = 0;
    unsigned height = 0;

    unsigned next() const { return (head + pending) % capacity; }

    NVENCSTATUS create_event(HANDLE& event, bool& registered) {
        event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (!event) return NV_ENC_ERR_OUT_OF_MEMORY;
        NV_ENC_EVENT_PARAMS params{};
        params.version = NV_ENC_EVENT_PARAMS_VER;
        params.completionEvent = event;
        auto status = api.nvEncRegisterAsyncEvent(encoder, &params);
        registered = status == NV_ENC_SUCCESS;
        return status;
    }

    void unregister_event(HANDLE event) {
        NV_ENC_EVENT_PARAMS params{};
        params.version = NV_ENC_EVENT_PARAMS_VER;
        params.completionEvent = event;
        api.nvEncUnregisterAsyncEvent(encoder, &params);
    }

    int retire(EncodeSlot& slot, ConsumeBitstream consume, void* context, bool& idr) {
        NV_ENC_LOCK_BITSTREAM lock{};
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = slot.bitstream;
        auto status = api.nvEncLockBitstream(encoder, &lock);
        if (status != NV_ENC_SUCCESS) return 11000 + status;
        idr = lock.pictureType == NV_ENC_PIC_TYPE_IDR;
        if (consume && lock.bitstreamSizeInBytes) consume(context, lock.bitstreamBufferPtr, lock.bitstreamSizeInBytes);
        status = api.nvEncUnlockBitstream(encoder, slot.bitstream);
        if (status != NV_ENC_SUCCESS) return 11000 + status;
        status = api.nvEncUnmapInputResource(encoder, slot.mapped);
        if (status != NV_ENC_SUCCESS) return 9000 + status;
        slot.mapped = nullptr;
        head = (head + 1) % capacity;
        --pending;
        return 0;
    }

    ~LanNvenc() {
        bool drained = true;
        if (submitted) {
            NV_ENC_PIC_PARAMS flush{};
            flush.version = NV_ENC_PIC_PARAMS_VER;
            flush.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
            flush.completionEvent = eos;
            drained = api.nvEncEncodePicture(encoder, &flush) == NV_ENC_SUCCESS
                && (!eos || WaitForSingleObject(eos, 2000) == WAIT_OBJECT_0);
        }
        while (drained && pending) {
            auto& slot = slots[head];
            bool idr = false;
            drained = (!slot.event || WaitForSingleObject(slot.event, 250) == WAIT_OBJECT_0)
                && retire(slot, nullptr, nullptr, idr) == 0;
        }
        // On device loss, retire the session before releasing the application's
        // textures or event handles. Normal teardown explicitly releases every resource.
        if (encoder && drained) {
            for (auto& slot : slots) {
                if (slot.registered_event) unregister_event(slot.event);
                if (slot.bitstream) api.nvEncDestroyBitstreamBuffer(encoder, slot.bitstream);
                if (slot.resource) api.nvEncUnregisterResource(encoder, slot.resource);
            }
            if (registered_eos) unregister_event(eos);
        }
        if (encoder) api.nvEncDestroyEncoder(encoder);
        for (auto& slot : slots) if (slot.event) CloseHandle(slot.event);
        if (eos) CloseHandle(eos);
        if (timer) CloseHandle(timer);
    }
};

static HMODULE driver_library() {
    // NVIDIA can run deferred D3D11 cleanup after the last encoder is destroyed.
    static HMODULE library = LoadLibraryExW(L"nvEncodeAPI64.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    return library;
}

static void keep_hybrid_graphics_shim_loaded() {
    HMODULE shim = nullptr;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN, L"nvdlistx.dll", &shim);
}

extern "C" void lan_keep_graphics_capture_loaded() {
    // A free-threaded capture pool can still have a callback on its way out
    // after Close. Retain the Windows capture provider for the host lifetime.
    HMODULE module = nullptr;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN, L"GraphicsCapture.dll", &module)) {
        static HMODULE system_module = LoadLibraryExW(L"GraphicsCapture.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
        (void)system_module;
    }
}

static int failure(LanNvenc* session, int stage, NVENCSTATUS status) {
    delete session;
    return stage * 1000 + static_cast<int>(status);
}

extern "C" int lan_nvenc_create(void* device, void* const* textures, unsigned texture_count,
    unsigned width, unsigned height, unsigned fps, unsigned bitrate, int spatial_aq,
    int asynchronous, void** result) {
    *result = nullptr;
    if (texture_count != LanNvenc::capacity) return 1000 + NV_ENC_ERR_INVALID_PARAM;
    auto* session = new(std::nothrow) LanNvenc;
    if (!session) return 1000 + NV_ENC_ERR_OUT_OF_MEMORY;
    const auto library = driver_library();
    if (!library) return failure(session, 1, NV_ENC_ERR_UNSUPPORTED_DEVICE);
    keep_hybrid_graphics_shim_loaded();
    using CreateApi = NVENCSTATUS (NVENCAPI*)(NV_ENCODE_API_FUNCTION_LIST*);
    using GetVersion = NVENCSTATUS (NVENCAPI*)(uint32_t*);
    const auto create_api = reinterpret_cast<CreateApi>(GetProcAddress(library, "NvEncodeAPICreateInstance"));
    const auto get_version = reinterpret_cast<GetVersion>(GetProcAddress(library, "NvEncodeAPIGetMaxSupportedVersion"));
    uint32_t supported = 0;
    if (!create_api || !get_version || get_version(&supported) != NV_ENC_SUCCESS
        || supported < ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION)) {
        return failure(session, 2, NV_ENC_ERR_INVALID_VERSION);
    }

    auto& api = session->api;
    api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    auto status = create_api(&api);
    if (status != NV_ENC_SUCCESS) return failure(session, 3, status);
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open{};
    open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    open.device = device;
    open.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    open.apiVersion = NVENCAPI_VERSION;
    status = api.nvEncOpenEncodeSessionEx(&open, &session->encoder);
    if (status != NV_ENC_SUCCESS) return failure(session, 4, status);

    session->asynchronous = asynchronous != 0;
    if (session->asynchronous) {
        NV_ENC_CAPS_PARAM caps{};
        caps.version = NV_ENC_CAPS_PARAM_VER;
        caps.capsToQuery = NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT;
        int supported_async = 0;
        status = api.nvEncGetEncodeCaps(session->encoder, NV_ENC_CODEC_H264_GUID, &caps, &supported_async);
        if (status != NV_ENC_SUCCESS) return failure(session, 12, status);
        if (!supported_async) return failure(session, 12, NV_ENC_ERR_UNSUPPORTED_PARAM);
    }

    NV_ENC_PRESET_CONFIG preset{};
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    status = api.nvEncGetEncodePresetConfigEx(session->encoder, NV_ENC_CODEC_H264_GUID,
        NV_ENC_PRESET_P3_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &preset);
    if (status != NV_ENC_SUCCESS) return failure(session, 5, status);
    auto& config = preset.presetCfg;
    config.profileGUID = NV_ENC_H264_PROFILE_BASELINE_GUID;
    config.gopLength = fps / 4;
    config.frameIntervalP = 1;
    config.frameFieldMode = NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    config.rcParams.averageBitRate = bitrate;
    config.rcParams.maxBitRate = bitrate;
    config.rcParams.vbvBufferSize = bitrate * 2 / fps;
    config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
    config.rcParams.enableLookahead = 0;
    config.rcParams.lookaheadDepth = 0;
    config.rcParams.zeroReorderDelay = 1;
    if (spatial_aq) {
        config.rcParams.enableAQ = 1;
        config.rcParams.aqStrength = 8;
    }
    auto& h264 = config.encodeCodecConfig.h264Config;
    h264.idrPeriod = config.gopLength;
    h264.repeatSPSPPS = 1;
    h264.disableSPSPPS = 0;
    h264.chromaFormatIDC = 1;
    h264.h264VUIParameters.videoSignalTypePresentFlag = 1;
    h264.h264VUIParameters.videoFullRangeFlag = 0;
    h264.h264VUIParameters.colourDescriptionPresentFlag = 1;
    h264.h264VUIParameters.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    h264.h264VUIParameters.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    h264.h264VUIParameters.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;

    NV_ENC_INITIALIZE_PARAMS init{};
    init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init.encodeGUID = NV_ENC_CODEC_H264_GUID;
    init.presetGUID = NV_ENC_PRESET_P3_GUID;
    init.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    init.encodeWidth = init.maxEncodeWidth = init.darWidth = width;
    init.encodeHeight = init.maxEncodeHeight = init.darHeight = height;
    init.frameRateNum = fps;
    init.frameRateDen = 1;
    init.enablePTD = 1;
    init.enableEncodeAsync = session->asynchronous;
    init.encodeConfig = &config;
    status = api.nvEncInitializeEncoder(session->encoder, &init);
    if (status != NV_ENC_SUCCESS) return failure(session, 6, status);
    keep_hybrid_graphics_shim_loaded();

    NV_ENC_REGISTER_RESOURCE register_texture{};
    register_texture.version = NV_ENC_REGISTER_RESOURCE_VER;
    register_texture.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    register_texture.width = width;
    register_texture.height = height;
    register_texture.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
    register_texture.bufferUsage = NV_ENC_INPUT_IMAGE;
    for (unsigned i = 0; i < texture_count; ++i) {
        auto& slot = session->slots[i];
        register_texture.resourceToRegister = textures[i];
        status = api.nvEncRegisterResource(session->encoder, &register_texture);
        if (status != NV_ENC_SUCCESS) return failure(session, 7, status);
        slot.resource = register_texture.registeredResource;
        NV_ENC_CREATE_BITSTREAM_BUFFER buffer{};
        buffer.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        status = api.nvEncCreateBitstreamBuffer(session->encoder, &buffer);
        if (status != NV_ENC_SUCCESS) return failure(session, 8, status);
        slot.bitstream = buffer.bitstreamBuffer;
        if (session->asynchronous) {
            status = session->create_event(slot.event, slot.registered_event);
            if (status != NV_ENC_SUCCESS) return failure(session, 13, status);
        }
    }
    if (session->asynchronous) {
        status = session->create_event(session->eos, session->registered_eos);
        if (status != NV_ENC_SUCCESS) return failure(session, 13, status);
        session->timer = CreateWaitableTimerExW(nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS);
        if (!session->timer) session->timer = CreateWaitableTimerW(nullptr, FALSE, nullptr);
        if (!session->timer) return failure(session, 14, NV_ENC_ERR_OUT_OF_MEMORY);
    }
    session->width = width;
    session->height = height;
    *result = session;
    return 0;
}

extern "C" unsigned lan_nvenc_queue(void* opaque, unsigned* next_slot) {
    auto* session = static_cast<LanNvenc*>(opaque);
    *next_slot = session->next();
    return session->pending;
}

extern "C" int lan_nvenc_submit(void* opaque, unsigned index, uint32_t frame, int keyframe) {
    auto* session = static_cast<LanNvenc*>(opaque);
    auto& api = session->api;
    if (index != session->next() || session->pending >= (session->asynchronous ? 2u : 1u))
        return 9000 + NV_ENC_ERR_INVALID_CALL;
    auto& slot = session->slots[index];
    if (slot.event && !ResetEvent(slot.event)) return 13000 + NV_ENC_ERR_INVALID_EVENT;
    NV_ENC_MAP_INPUT_RESOURCE map{};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = slot.resource;
    auto status = api.nvEncMapInputResource(session->encoder, &map);
    if (status != NV_ENC_SUCCESS) return 9000 + status;

    NV_ENC_PIC_PARAMS picture{};
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.inputWidth = session->width;
    picture.inputHeight = session->height;
    picture.inputTimeStamp = frame;
    picture.inputDuration = 1;
    picture.inputBuffer = map.mappedResource;
    picture.bufferFmt = map.mappedBufferFmt;
    picture.outputBitstream = slot.bitstream;
    picture.completionEvent = slot.event;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    if (keyframe) picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    status = api.nvEncEncodePicture(session->encoder, &picture);
    if (status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT) {
        api.nvEncUnmapInputResource(session->encoder, map.mappedResource);
        if (status == NV_ENC_ERR_ENCODER_BUSY) return 1; // This input was not accepted.
        return 10000 + status;
    }
    slot.mapped = map.mappedResource;
    slot.frame = frame;
    session->submitted = true;
    ++session->pending;
    // No frame reordering is configured. A sync driver asking for additional
    // input cannot deliver a seed frame; preserve its mapping until EOS.
    if (!session->asynchronous && status == NV_ENC_ERR_NEED_MORE_INPUT) return 10000 + status;
    return 0;
}

struct OutputInfo {
    uint32_t frame, slot, idr;
};

extern "C" int lan_nvenc_receive(void* opaque, uint32_t wait_us, HANDLE frame_event, OutputInfo* info,
    ConsumeBitstream consume, void* context) {
    auto* session = static_cast<LanNvenc*>(opaque);
    if (!session->pending) return 1;
    auto& slot = session->slots[session->head];
    if (session->asynchronous) {
        DWORD ready = WaitForSingleObject(slot.event, 0);
        if (ready == WAIT_TIMEOUT && wait_us) {
            // Wake for either the oldest bitstream or the next capture deadline.
            LARGE_INTEGER due{};
            due.QuadPart = -10LL * wait_us;
            if (!SetWaitableTimer(session->timer, &due, 0, nullptr, nullptr, FALSE))
                return 14000 + NV_ENC_ERR_GENERIC;
            HANDLE events[] = {slot.event, session->timer,frame_event};
            ready = WaitForMultipleObjects(frame_event ? 3 : 2, events, FALSE, INFINITE);
        }
        if (ready == WAIT_TIMEOUT || ready == WAIT_OBJECT_0 + 1 || ready == WAIT_OBJECT_0 + 2) return 1;
        if (ready != WAIT_OBJECT_0) return 13000 + NV_ENC_ERR_INVALID_EVENT;
    }
    *info = {slot.frame, session->head, 0};
    bool idr = false;
    const int result = session->retire(slot, consume, context, idr);
    info->idr = idr ? 1u : 0u;
    return result;
}

extern "C" void lan_nvenc_destroy(void* opaque) {
    delete static_cast<LanNvenc*>(opaque);
}
