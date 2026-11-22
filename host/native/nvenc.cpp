#include <windows.h>
#include <new>
#include "nvEncodeAPI.h"

// The Rust capture thread owns the device, its registered texture and this session.
struct LanNvenc {
    HMODULE library = nullptr;
    NV_ENCODE_API_FUNCTION_LIST api{};
    void* encoder = nullptr;
    NV_ENC_REGISTERED_PTR resource = nullptr;
    NV_ENC_OUTPUT_PTR bitstream = nullptr;
    unsigned width = 0;
    unsigned height = 0;
    unsigned fps = 0;

    ~LanNvenc() {
        if (bitstream) api.nvEncDestroyBitstreamBuffer(encoder, bitstream);
        if (resource) api.nvEncUnregisterResource(encoder, resource);
        if (encoder) api.nvEncDestroyEncoder(encoder);
        if (library) FreeLibrary(library);
    }
};

static int failure(LanNvenc* session, int stage, NVENCSTATUS status) {
    delete session;
    return stage * 1000 + static_cast<int>(status);
}

extern "C" int lan_nvenc_create(void* device, void* texture, unsigned width, unsigned height,
    unsigned fps, unsigned bitrate, void** result) {
    *result = nullptr;
    auto* session = new(std::nothrow) LanNvenc;
    if (!session) return 1000 + NV_ENC_ERR_OUT_OF_MEMORY;
    session->library = LoadLibraryExW(L"nvEncodeAPI64.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!session->library) return failure(session, 1, NV_ENC_ERR_UNSUPPORTED_DEVICE);
    using CreateApi = NVENCSTATUS (NVENCAPI*)(NV_ENCODE_API_FUNCTION_LIST*);
    using GetVersion = NVENCSTATUS (NVENCAPI*)(uint32_t*);
    const auto create_api = reinterpret_cast<CreateApi>(GetProcAddress(session->library, "NvEncodeAPICreateInstance"));
    const auto get_version = reinterpret_cast<GetVersion>(GetProcAddress(session->library, "NvEncodeAPIGetMaxSupportedVersion"));
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
    auto& h264 = config.encodeCodecConfig.h264Config;
    h264.idrPeriod = config.gopLength;
    h264.repeatSPSPPS = 1;
    h264.disableSPSPPS = 0;
    h264.chromaFormatIDC = 1;
    h264.h264VUIParameters.videoSignalTypePresentFlag = 1;
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
    init.enableEncodeAsync = 0;
    init.encodeConfig = &config;
    status = api.nvEncInitializeEncoder(session->encoder, &init);
    if (status != NV_ENC_SUCCESS) return failure(session, 6, status);

    NV_ENC_REGISTER_RESOURCE register_texture{};
    register_texture.version = NV_ENC_REGISTER_RESOURCE_VER;
    register_texture.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    register_texture.resourceToRegister = texture;
    register_texture.width = width;
    register_texture.height = height;
    register_texture.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;
    register_texture.bufferUsage = NV_ENC_INPUT_IMAGE;
    status = api.nvEncRegisterResource(session->encoder, &register_texture);
    if (status != NV_ENC_SUCCESS) return failure(session, 7, status);
    session->resource = register_texture.registeredResource;

    NV_ENC_CREATE_BITSTREAM_BUFFER buffer{};
    buffer.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    status = api.nvEncCreateBitstreamBuffer(session->encoder, &buffer);
    if (status != NV_ENC_SUCCESS) return failure(session, 8, status);
    session->bitstream = buffer.bitstreamBuffer;
    session->width = width;
    session->height = height;
    session->fps = fps;
    *result = session;
    return 0;
}

extern "C" int lan_nvenc_picture(void* opaque, uint64_t frame, int keyframe,
    void (*consume)(void*, const void*, uint32_t), void* context) {
    auto* session = static_cast<LanNvenc*>(opaque);
    auto& api = session->api;
    NV_ENC_MAP_INPUT_RESOURCE map{};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = session->resource;
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
    picture.outputBitstream = session->bitstream;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    if (keyframe) picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    status = api.nvEncEncodePicture(session->encoder, &picture);
    if (status != NV_ENC_SUCCESS) {
        api.nvEncUnmapInputResource(session->encoder, map.mappedResource);
        if (status == NV_ENC_ERR_NEED_MORE_INPUT || status == NV_ENC_ERR_ENCODER_BUSY) return 0;
        return 10000 + status;
    }

    NV_ENC_LOCK_BITSTREAM lock{};
    lock.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock.outputBitstream = session->bitstream;
    status = api.nvEncLockBitstream(session->encoder, &lock);
    if (status == NV_ENC_SUCCESS) {
        if (lock.bitstreamSizeInBytes) consume(context, lock.bitstreamBufferPtr, lock.bitstreamSizeInBytes);
        api.nvEncUnlockBitstream(session->encoder, session->bitstream);
    }
    api.nvEncUnmapInputResource(session->encoder, map.mappedResource);
    return status == NV_ENC_SUCCESS ? 0 : 11000 + status;
}

extern "C" void lan_nvenc_destroy(void* opaque) {
    delete static_cast<LanNvenc*>(opaque);
}
