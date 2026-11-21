// fan_control.cpp
//
// Fan / SSR control for Phidget VINT outputs on the SBC4's built-in hub.
// Default hardware:
//   OUT1100 (4ch digital output w/ PWM) on hub port 2  -> fans @ duty cycle
//   REL1100 (4ch SSR)                    on hub port 5  -> SSR on/off
//
// By default every channel on both devices is started (fans @ 50%, SSR on) and
// held, with a per-channel hardware failsafe that drops outputs to OFF if this
// program dies. CLI flags below override ports/channels, duty, and the open
// model. No TACH, no temperature yet -- but the device table is structured so
// a TMP1101 (TemperatureSensor) and DAQ1200 (DigitalInput) can be added later
// as new rows without touching the core loop.
//
// Usage:
//   fan_control [--fan PORT[:CHANS]] [--ssr PORT[:CHANS]] [--duty D]
//               [--static] [--failsafe-ms N] [--help]
//
//   --fan PORT[:CHANS]   OUT1100 location. Default 2:0,1,2,3. ":CHANS" optional
//                        (e.g. --fan 2 = all 4 channels; --fan 2:0,1 = ch 0,1).
//   --ssr PORT[:CHANS]   REL1100 location. Default 5:0,1,2,3.
//   --duty D             Fan PWM duty, 0.0-1.0. Default 0.5.
//   --static             Require all selected channels present at startup
//                        (openWaitForAttachment); exit nonzero if any is
//                        missing. Default is dynamic: non-blocking open, each
//                        channel powers on as it attaches and re-powers on
//                        re-attach. Both modes arm the failsafe and hold.
//   --failsafe-ms N      Failsafe window in ms. Default 1000.
//   --help               Print usage and exit.
//
// Build:  g++ -std=c++17 fan_control.cpp -lphidget22 -lpthread -o fan_control

#include <phidget22.h>

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

namespace {

// ---- Channel kinds -----------------------------------------------------------
// Outputs now; inputs (Temp/DigitalInput) are stubbed for later extension.
enum class Kind { FanPwm, Ssr /*, Temp, DigitalIn */ };

const char* KindLabel(Kind k) {
    switch (k) {
        case Kind::FanPwm: return "FAN";
        case Kind::Ssr:    return "SSR";
    }
    return "?";
}

// ---- Config (filled from defaults, then CLI) ---------------------------------
struct DeviceSpec {
    Kind kind;
    int  hubPort;
    std::vector<int> channels;   // empty here is never used; populated to 0..3
    bool failsafe;
};

constexpr int    kSBC4Serial      = -1;      // TODO: real SBC serial for specificity
constexpr double kDefaultDuty     = 0.50;
constexpr uint32_t kDefaultFailMs = 1000;
constexpr int    kResetFailWarn   = 5;

struct Config {
    DeviceSpec fan { Kind::FanPwm, 2, {0,1,2,3}, true };
    DeviceSpec ssr { Kind::Ssr,    5, {0,1,2,3}, true };
    double     duty       = kDefaultDuty;
    bool       dynamic    = true;            // --static flips to false
    uint32_t   failsafeMs = kDefaultFailMs;
    bool       fanEnabled = true;
    bool       ssrEnabled = true;
};

// ---- Globals -----------------------------------------------------------------
std::atomic<bool> g_running{true};
void OnSignal(int) { g_running.store(false); }

uint32_t     g_failsafeMs = kDefaultFailMs;
double       g_duty       = kDefaultDuty;
std::chrono::milliseconds g_resetInterval{kDefaultFailMs / 3};

const char* ErrStr(PhidgetReturnCode rc) {
    const char* d = nullptr;
    Phidget_getErrorDescription(rc, &d);
    return d ? d : "?";
}

// ---- Per-channel runtime state ----------------------------------------------
struct Channel {
    PhidgetDigitalOutputHandle handle = nullptr;
    Kind kind = Kind::FanPwm;
    int  hubPort = 0, channel = 0;
    bool wantFailsafe = true;
    bool attached = false;   // guarded by g_mutex
    int  failStreak = 0;     // guarded by g_mutex
    const char* label() const { return KindLabel(kind); }
};

std::mutex g_mutex;
std::vector<Channel> g_channels;

// Drive a channel to its target output, then (optionally) arm its failsafe.
bool DriveChannel(Channel& c) {
    PhidgetReturnCode rc;
    if (c.kind == Kind::FanPwm)
        rc = PhidgetDigitalOutput_setDutyCycle(c.handle, g_duty);  // PWM 0.0-1.0
    else
        rc = PhidgetDigitalOutput_setState(c.handle, 1);           // SSR on
    if (rc != EPHIDGET_OK) {
        std::fprintf(stderr, "ERROR: %s port %d ch %d set-output failed: %s (%d)\n",
                     c.label(), c.hubPort, c.channel, ErrStr(rc), rc);
        return false;
    }
    if (c.wantFailsafe) {
        rc = PhidgetDigitalOutput_enableFailsafe(c.handle, g_failsafeMs);
        if (rc != EPHIDGET_OK) {
            std::fprintf(stderr, "ERROR: %s port %d ch %d enableFailsafe failed: %s (%d)\n",
                         c.label(), c.hubPort, c.channel, ErrStr(rc), rc);
            return false;
        }
    }
    {
        std::lock_guard<std::mutex> lk(g_mutex);
        c.failStreak = 0;
        c.attached = true;
    }
    if (c.kind == Kind::FanPwm)
        std::fprintf(stderr, "%s port %d ch %d -> %.0f%%\n",
                     c.label(), c.hubPort, c.channel, g_duty * 100.0);
    else
        std::fprintf(stderr, "%s port %d ch %d -> ON\n", c.label(), c.hubPort, c.channel);
    return true;
}

// ---- Event handlers (dynamic mode) ------------------------------------------
void CCONV OnAttach(PhidgetHandle /*h*/, void* ctx) {
    auto* c = static_cast<Channel*>(ctx);
    std::fprintf(stderr, "ATTACH: %s port %d ch %d\n", c->label(), c->hubPort, c->channel);
    DriveChannel(*c);
}

void CCONV OnDetach(PhidgetHandle /*h*/, void* ctx) {
    auto* c = static_cast<Channel*>(ctx);
    {
        std::lock_guard<std::mutex> lk(g_mutex);
        c->attached = false;
    }
    std::fprintf(stderr, "DETACH: %s port %d ch %d\n", c->label(), c->hubPort, c->channel);
    // Detached device is unpowered on VINT -> output already off. open() keeps
    // retrying; re-attach re-runs OnAttach and re-drives it.
}

void CCONV OnError(PhidgetHandle /*h*/, void* ctx, Phidget_ErrorEventCode code,
                   const char* description) {
    auto* c = static_cast<Channel*>(ctx);
    std::fprintf(stderr, "ERROR: %s port %d ch %d: %s (%d)\n",
                 c->label(), c->hubPort, c->channel, description ? description : "?", code);
}

// ---- CLI parsing -------------------------------------------------------------
void PrintUsage(const char* argv0) {
    std::printf(
        "Usage: %s [--fan PORT[:CHANS]] [--ssr PORT[:CHANS]] [--duty D]\n"
        "          [--static] [--failsafe-ms N] [--help]\n\n"
        "  --fan PORT[:CHANS]  OUT1100 location (default 2:0,1,2,3). Omit :CHANS\n"
        "                      for all 4 channels, e.g. --fan 2 or --fan 2:0,1.\n"
        "  --ssr PORT[:CHANS]  REL1100 location (default 5:0,1,2,3).\n"
        "  --duty D            Fan PWM duty 0.0-1.0 (default 0.5).\n"
        "  --static            Require all selected channels present at startup\n"
        "                      (exit nonzero if any missing). Default is dynamic.\n"
        "  --failsafe-ms N     Failsafe window in ms (default 1000).\n"
        "  --help              This message.\n",
        argv0);
}

// Parse "PORT" or "PORT:c1,c2,...". Returns false on malformed input.
bool ParsePortChannels(const char* s, int& port, std::vector<int>& chans) {
    const char* colon = std::strchr(s, ':');
    std::string portStr = colon ? std::string(s, colon - s) : std::string(s);
    char* end = nullptr;
    long p = std::strtol(portStr.c_str(), &end, 10);
    if (end == portStr.c_str() || *end != '\0' || p < 0 || p > 5) return false;
    port = static_cast<int>(p);

    if (!colon) { chans = {0, 1, 2, 3}; return true; }   // no list -> all 4
    chans.clear();
    const char* cur = colon + 1;
    while (*cur) {
        char* e = nullptr;
        long ch = std::strtol(cur, &e, 10);
        if (e == cur || ch < 0 || ch > 3) return false;
        chans.push_back(static_cast<int>(ch));
        cur = e;
        if (*cur == ',') ++cur;
        else if (*cur != '\0') return false;
    }
    return !chans.empty();
}

bool NeedArg(int i, int argc, const char* flag) {
    if (i + 1 >= argc) {
        std::fprintf(stderr, "ERROR: %s needs a value\n", flag);
        return false;
    }
    return true;
}

// Returns true to proceed, false to exit; sets exitCode.
bool ParseArgs(int argc, char** argv, Config& cfg, int& exitCode) {
    exitCode = 0;
    for (int i = 1; i < argc; ++i) {
        const char* a = argv[i];
        if (!std::strcmp(a, "--help") || !std::strcmp(a, "-h")) {
            PrintUsage(argv[0]); return false;
        } else if (!std::strcmp(a, "--static")) {
            cfg.dynamic = false;
        } else if (!std::strcmp(a, "--fan")) {
            if (!NeedArg(i, argc, a)) { exitCode = 2; return false; }
            if (!ParsePortChannels(argv[++i], cfg.fan.hubPort, cfg.fan.channels)) {
                std::fprintf(stderr, "ERROR: bad --fan '%s' (want PORT[:0,1,2,3])\n", argv[i]);
                exitCode = 2; return false;
            }
        } else if (!std::strcmp(a, "--ssr")) {
            if (!NeedArg(i, argc, a)) { exitCode = 2; return false; }
            if (!ParsePortChannels(argv[++i], cfg.ssr.hubPort, cfg.ssr.channels)) {
                std::fprintf(stderr, "ERROR: bad --ssr '%s' (want PORT[:0,1,2,3])\n", argv[i]);
                exitCode = 2; return false;
            }
        } else if (!std::strcmp(a, "--duty")) {
            if (!NeedArg(i, argc, a)) { exitCode = 2; return false; }
            char* end = nullptr;
            double d = std::strtod(argv[++i], &end);
            if (end == argv[i] || *end != '\0' || d < 0.0 || d > 1.0) {
                std::fprintf(stderr, "ERROR: bad --duty '%s' (want 0.0-1.0)\n", argv[i]);
                exitCode = 2; return false;
            }
            cfg.duty = d;
        } else if (!std::strcmp(a, "--failsafe-ms")) {
            if (!NeedArg(i, argc, a)) { exitCode = 2; return false; }
            char* end = nullptr;
            long n = std::strtol(argv[++i], &end, 10);
            if (end == argv[i] || *end != '\0' || n < 50 || n > 60000) {
                std::fprintf(stderr, "ERROR: bad --failsafe-ms '%s' (want 50-60000)\n", argv[i]);
                exitCode = 2; return false;
            }
            cfg.failsafeMs = static_cast<uint32_t>(n);
        } else {
            std::fprintf(stderr, "ERROR: unknown argument '%s' (try --help)\n", a);
            exitCode = 2; return false;
        }
    }
    return true;
}

void BuildChannels(const Config& cfg) {
    auto add = [](const DeviceSpec& d) {
        for (int ch : d.channels) {
            g_channels.push_back(Channel{});
            Channel& c = g_channels.back();
            c.kind = d.kind;
            c.hubPort = d.hubPort;
            c.channel = ch;
            c.wantFailsafe = d.failsafe;
        }
    };
    if (cfg.fanEnabled) add(cfg.fan);
    if (cfg.ssrEnabled) add(cfg.ssr);
}

} // namespace

int main(int argc, char** argv) {
    Config cfg;
    int exitCode = 0;
    if (!ParseArgs(argc, argv, cfg, exitCode)) return exitCode;

    g_duty       = cfg.duty;
    g_failsafeMs = cfg.failsafeMs;
    g_resetInterval = std::chrono::milliseconds(cfg.failsafeMs / 3);

    std::signal(SIGINT,  OnSignal);
    std::signal(SIGTERM, OnSignal);

    BuildChannels(cfg);
    const int n = static_cast<int>(g_channels.size());

    // ---- Create + address + register handlers -------------------------------
    for (auto& c : g_channels) {
        if (PhidgetDigitalOutput_create(&c.handle) != EPHIDGET_OK) {
            std::fprintf(stderr, "FATAL: create failed\n"); return 1;
        }
        auto h = reinterpret_cast<PhidgetHandle>(c.handle);
        Phidget_setDeviceSerialNumber(h, kSBC4Serial);
        Phidget_setHubPort(h, c.hubPort);
        Phidget_setChannel(h, c.channel);
        Phidget_setOnAttachHandler(h, OnAttach, &c);
        Phidget_setOnDetachHandler(h, OnDetach, &c);
        Phidget_setOnErrorHandler(h,  OnError,  &c);
    }

    if (cfg.dynamic) {
        // ---- Dynamic: non-blocking open; channels power on via OnAttach -----
        for (auto& c : g_channels) {
            PhidgetReturnCode rc = Phidget_open(reinterpret_cast<PhidgetHandle>(c.handle));
            if (rc != EPHIDGET_OK) {
                std::fprintf(stderr, "FATAL: open failed: %s (%d)\n", ErrStr(rc), rc);
                return 1;
            }
        }
        std::fprintf(stderr,
            "Started (dynamic). %d channels opened; fans @ %.0f%%, SSR on as they "
            "attach. Failsafe %ums.\n", n, g_duty * 100.0, g_failsafeMs);
    } else {
        // ---- Static: require every selected channel present at startup ------
        for (auto& c : g_channels) {
            auto h = reinterpret_cast<PhidgetHandle>(c.handle);
            PhidgetReturnCode rc = Phidget_openWaitForAttachment(h, 5000);
            if (rc != EPHIDGET_OK) {
                std::fprintf(stderr,
                    "FATAL (static): %s port %d ch %d not attached: %s (%d)\n",
                    c.label(), c.hubPort, c.channel, ErrStr(rc), rc);
                return 1;   // strict: a missing channel is fatal in static mode
            }
            // Attach handler already fired and drove it; nothing else to do here.
        }
        std::fprintf(stderr,
            "Started (static). All %d channels present; fans @ %.0f%%, SSR on. "
            "Failsafe %ums.\n", n, g_duty * 100.0, g_failsafeMs);
    }

    // ---- Heartbeat loop: feed failsafe on attached channels -----------------
    while (g_running.load()) {
        {
            std::lock_guard<std::mutex> lk(g_mutex);
            for (auto& c : g_channels) {
                if (!c.attached || !c.wantFailsafe) continue;
                PhidgetReturnCode rc = PhidgetDigitalOutput_resetFailsafe(c.handle);
                if (rc == EPHIDGET_OK) {
                    c.failStreak = 0;
                } else if (++c.failStreak == kResetFailWarn) {
                    std::fprintf(stderr, "WARN: %s port %d ch %d failsafe reset failing: %s (%d)\n",
                                 c.label(), c.hubPort, c.channel, ErrStr(rc), rc);
                }
            }
        }
        std::this_thread::sleep_for(g_resetInterval);
    }

    // ---- Graceful shutdown ---------------------------------------------------
    std::fprintf(stderr, "Shutting down: outputs off\n");
    for (auto& c : g_channels) {
        if (c.kind == Kind::FanPwm) PhidgetDigitalOutput_setDutyCycle(c.handle, 0.0);
        else                        PhidgetDigitalOutput_setState(c.handle, 0);
        auto h = reinterpret_cast<PhidgetHandle>(c.handle);
        Phidget_close(h);
        PhidgetDigitalOutput_delete(&c.handle);
    }
    return 0;
}

