// core.h -- rack_controller logic, independent of Phidget22 (see hal.h).
//
// One unified channel model (Relay | FanPwm | Ssr), one mutex, one heartbeat,
// the socket protocol and the Unix-socket server. rack_controller.cpp wires
// this to the real Phidget HAL; tests/ wire it to a fake.
#pragma once

#include "hal.h"

#include <atomic>
#include <chrono>
#include <cstdint>
#include <functional>
#include <mutex>
#include <string>
#include <vector>

namespace rc {

// ---- Constants (relay values are relay_controller's, verbatim) --------------
constexpr int      kDefaultSerial     = 592109;   // the SBC4's HUB0004
constexpr uint32_t kDefaultFailsafeMs = 1000;
constexpr double   kDefaultDuty       = 0.50;
constexpr uint32_t kAttachTimeoutMs   = 5000;
constexpr int      kResetFailWarn     = 5;
constexpr std::chrono::milliseconds kStaggerDelay{2500};
constexpr size_t   kMaxRequest        = 128;      // bytes per request line, excl. '\n'
constexpr int      kRelayCount        = 8;
constexpr std::chrono::seconds kReopenRetry{5};  // tripped-channel reopen retry
constexpr int      kRelayPorts[2]     = {3, 4};   // REL1000 x2: relay ch 0-3, 4-7
const char* const  kDefaultSocket     = "/run/rack_controller/ctl.sock";
const char* const  kSocketGroup       = "rackctl";   // socket + its dir: root:rackctl

// ---- Channel model -----------------------------------------------------------
enum class Kind { Relay, FanPwm, Ssr };
const char* KindLabel(Kind k);  // log label: RELAY / FAN / SSR
const char* KindWord(Kind k);   // protocol word: relay / fan / ssr

struct Channel {
    Kind kind = Kind::Relay;
    int  number = 0;    // protocol channel: relay 0-7, fan/ssr device channel 0-3
    int  hubPort = 0, channel = 0;
    // Everything below is guarded by Controller's mutex.
    bool   attached = false;
    bool   armed = false;     // failsafe enabled; the heartbeat feeds it
    bool   tripped = false;   // failsafe tripped: rejects input until re-attach
    int    failStreak = 0;
    bool   cmdOn = true;      // relay: closed (load on); ssr: on. Not persisted.
                              // A relay detach or trip sets it false (really open).
    double duty = kDefaultDuty;  // fan only
    uint64_t epoch = 0;       // bumped on attach/detach/trip; stale commands don't stick
    bool   reopened = false;  // a reopen after a trip is in flight
    std::chrono::steady_clock::time_point lastReopen{};
};

// ---- Config (defaults, then CLI) ---------------------------------------------
struct DeviceSpec {
    int hubPort;
    std::vector<int> channels;
};

struct Config {
    int         serial = kDefaultSerial;
    DeviceSpec  fan{2, {0, 1, 2, 3}};   // OUT1100
    DeviceSpec  ssr{5, {0, 1, 2, 3}};   // REL1100
    double      duty = kDefaultDuty;
    uint32_t    failsafeMs = kDefaultFailsafeMs;
    std::string socketPath = kDefaultSocket;
    bool        startOpen = false;   // --start-open: every relay commanded open at start
    unsigned    relayMask = 0xFF;    // --relays: bit i set = relay i closed at start (default all)
};

void PrintUsage(const char* argv0);
// fan_control-style parsing. Returns true to run; false to exit with exitCode.
bool ParseArgs(int argc, char** argv, Config& cfg, int& exitCode);

// ---- Socket protocol ---------------------------------------------------------
enum class ReqType { Open, Close, Fan, Ssr, State };
struct Request {
    ReqType type = ReqType::State;
    int     ch = 0;
    double  duty = 0.0;
    bool    on = false;
};
// Strict parse of one request line (no newline). On failure returns false and
// sets err to the reason (sent as "err <reason>").
bool ParseRequest(const std::string& line, Request& out, std::string& err);

// ---- Controller --------------------------------------------------------------
class Controller : public HalListener {
public:
    // Called by Check() on a fatal startup error; must not return (the
    // default prints nothing extra and std::exit(1)s, as relay_controller did).
    using FatalFn = std::function<void()>;
    // Waits one stagger slot. The default sleeps in short steps and returns
    // early once `running` goes false.
    using SleepFn = std::function<void(std::chrono::milliseconds)>;

    Controller(Hal& hal, const Config& cfg, std::atomic<bool>& running);

    void SetFatal(FatalFn f) { fatal_ = std::move(f); }
    void SetSleep(SleepFn f) { sleep_ = std::move(f); }

    // ---- Startup, in this order (see rack_controller.cpp) ----
    void CreateAll();           // one create loop for every kind; fatal on error
    void OpenRelaysBlocking();  // relay_controller: openWaitForAttachment, start FALSE
    void OpenDynamic();         // fan_control: non-blocking open; driven on attach
    void RunStagger();          // one-time 2.5 s index-based relay power-on

    // ---- Heartbeat ----
    void HeartbeatTick();       // reset the failsafe on every armed channel
    void HeartbeatLoop();       // tick every failsafeMs/3 until running is false

    // ---- Reopen failsafe-tripped channels (main thread, every ~100 ms) ----
    void MaintenanceTick();

    // ---- Graceful shutdown: relays open, fans 0, SSR off, handles closed ----
    void Shutdown();

    // ---- Socket requests: reply lines, without newlines ----
    std::vector<std::string> Handle(const std::string& line);

    // ---- HalListener (Phidget event thread) ----
    void OnAttach(int idx) override;
    void OnDetach(int idx) override;
    void OnError(int idx, int code, const char* description, bool failsafeTrip) override;

    // ---- Introspection (tests) ----
    Channel Snapshot(int idx) const;
    int Size() const { return static_cast<int>(ch_.size()); }
    int RelayIndex(int number) const { return number; }  // relays come first
    int Find(Kind k, int number) const;                  // -1 if not configured

private:
    void Check(Rc rc, const char* what);  // relay_controller's fatal check
    bool Drive(int idx, uint64_t epoch);  // fan_control's DriveChannel
    bool Apply(int idx, bool arm, const std::function<Rc()>& set, const char* what,
               const std::function<void(Channel&)>& record, std::string& err);
    bool UsableLocked(int idx, std::string& err) const;
    void NoteFailureLocked(int idx, Rc rc);
    void MarkTrippedLocked(int idx);
    std::string FailLocked(int idx, Rc rc, const char* what);  // logs; returns err text
    std::string Describe(int idx) const;  // "relay 3 (port 3 ch 3)"
    std::string Name(int idx) const;      // "relay 3"

    Hal& hal_;
    const Config cfg_;
    std::atomic<bool>& running_;
    std::chrono::milliseconds resetInterval_;
    FatalFn fatal_;
    SleepFn sleep_;

    // The one channel-state mutex. Never held across a HAL call.
    mutable std::mutex mu_;
    // Serializes output changes (commands, stagger, attach drive). Taken
    // before mu_; the heartbeat never needs it.
    std::mutex cmdMu_;
    std::vector<Channel> ch_;  // relays 0-7 first, then fans, then SSR
};

// ---- Unix-socket server ------------------------------------------------------
// Runs on its own thread until `running` goes false. Socket and request errors
// are logged and never end the process. Several clients, many requests each.
class SocketServer {
public:
    SocketServer(Controller& ctl, std::string path, std::atomic<bool>& running);
    void Run();
    // Max simultaneous clients; extra connections get "err too many clients".
    static constexpr size_t kMaxClients = 16;

private:
    bool Bind();

    Controller& ctl_;
    const std::string path_;
    std::atomic<bool>& running_;
    int listenFd_ = -1;
};

} // namespace rc
