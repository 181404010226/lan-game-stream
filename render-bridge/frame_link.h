#pragma once
#include <windows.h>
#include <cstdint>
#include <string>

namespace frame_link {
constexpr unsigned slots = 3;
constexpr LONG version = 1;
constexpr LONG magic = 0x4C474631; // LGF1
enum Mode : LONG { direct = 1, copy = 2 };
struct alignas(8) Slot {
    volatile LONG64 handle;
    volatile LONG64 sequence;
};
struct alignas(8) Control {
    volatile LONG signature, abi, pid, serial;
    volatile LONG request, requested_width, requested_height, requested_mode;
    volatile LONG64 heartbeat;
    volatile LONG ready, active_mode, width, height;
    volatile LONG error;
    Slot frames[slots];
};
inline LONG read(volatile LONG &n) { return InterlockedCompareExchange(&n, 0, 0); }
inline LONG64 read(volatile LONG64 &n) { return InterlockedCompareExchange64(&n, 0, 0); }
inline void put(volatile LONG &n, LONG v) { InterlockedExchange(&n, v); }
inline void put(volatile LONG64 &n, LONG64 v) { InterlockedExchange64(&n, v); }
inline std::wstring name(DWORD pid) { return L"Local\\LanGameStream.Frame.v1." + std::to_wstring(pid); }
inline bool dimensions(LONG w, LONG h) {
    return w >= 64 && w <= 3840 && h >= 64 && h <= 2160 && !(w & 1) && !(h & 1);
}
inline LONG64 clock_us() {
    LARGE_INTEGER t, f;
    QueryPerformanceCounter(&t);
    QueryPerformanceFrequency(&f);
    return (t.QuadPart / f.QuadPart) * 1000000 + (t.QuadPart % f.QuadPart) * 1000000 / f.QuadPart;
}
inline bool requested(Control *c) {
    return c && read(c->request) && GetTickCount64() - static_cast<ULONGLONG>(read(c->heartbeat)) < 4000;
}
struct Mapping {
    HANDLE file{};
    Control *c{};
    ~Mapping() {
        if (c)
            UnmapViewOfFile(c);
        if (file)
            CloseHandle(file);
    }
    bool map(HANDLE opened) {
        file = opened;
        if (!file)
            return false;
        c = static_cast<Control *>(
            MapViewOfFile(file, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, sizeof(Control)));
        return c != nullptr;
    }
    Mapping() = default;
    Mapping(const Mapping &) = delete;
    Mapping &operator=(const Mapping &) = delete;
};
} // namespace frame_link
