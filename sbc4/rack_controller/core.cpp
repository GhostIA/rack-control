// core.cpp -- rack_controller logic. No Phidget22 includes: all hardware goes
// through rc::Hal. See core.h and rack_controller.cpp.

#include "core.h"

#include <cerrno>
#include <exception>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <thread>

#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

namespace rc {

const char* KindLabel(Kind k) {
    switch (k) {
        case Kind::Relay:  return "RELAY";
        case Kind::FanPwm: return "FAN";
        case Kind::Ssr:    return "SSR";
    }
    return "?";
}

const char* KindWord(Kind k) {
    switch (k) {
        case Kind::Relay:  return "relay";
        case Kind::FanPwm: return "fan";
        case Kind::Ssr:    return "ssr";
    }
    return "?";
}

// =============================================================================
// CLI (fan_control's parsing style)
// =============================================================================
void PrintUsage(const char* argv0) {
    std::printf(
        "Usage: %s [--serial N] [--fan PORT[:CHANS]] [--ssr PORT[:CHANS]]\n"
        "          [--duty D] [--failsafe-ms N] [--socket PATH] [--help]\n\n"
        "Owns the SBC4's whole VINT hub: relays (REL1000 x2, hub ports 3 and 4,\n"
        "relay ch 0-7), fans (OUT1100 PWM) and SSR (REL1100), plus a local\n"
        "control socket.\n\n"
        "  --serial N          Hub serial number (default %d; -1 = any).\n"
        "  --fan PORT[:CHANS]  OUT1100 location (default 2:0,1,2,3). Omit :CHANS\n"
        "                      for all 4 channels, e.g. --fan 2 or --fan 2:0,1.\n"
        "  --ssr PORT[:CHANS]  REL1100 location (default 5:0,1,2,3).\n"
        "  --duty D            Initial fan PWM duty 0.0-1.0 (default 0.5).\n"
        "  --failsafe-ms N     Failsafe window in ms, all channels (default %u).\n"
        "  --socket PATH       Control socket (default %s).\n"
        "  --help              This message.\n",
        argv0, kDefaultSerial, kDefaultFailsafeMs, kDefaultSocket);
}

namespace {

// Parse "PORT" or "PORT:c1,c2,...". Returns false on malformed input.
// Strict: PORT is one digit 0-5, CHANS one or more distinct digits 0-3
// separated by single commas. No signs, spaces or trailing comma.
bool ParsePortChannels(const char* s, int& port, std::vector<int>& chans) {
    if (s[0] < '0' || s[0] > '5') return false;
    const int p = s[0] - '0';
    if (s[1] == '\0') { port = p; chans = {0, 1, 2, 3}; return true; }   // no list -> all 4
    if (s[1] != ':') return false;
    std::vector<int> out;
    const char* cur = s + 2;
    for (;;) {
        if (*cur < '0' || *cur > '3') return false;
        const int ch = *cur - '0';
        for (int have : out)
            if (have == ch) return false;   // a channel opened twice would contend
        out.push_back(ch);
        ++cur;
        if (*cur == '\0') break;
        if (*cur != ',') return false;
        ++cur;
    }
    port = p;
    chans = out;
    return true;
}

bool NeedArg(int i, int argc, const char* flag) {
    if (i + 1 >= argc) {
        std::fprintf(stderr, "ERROR: %s needs a value\n", flag);
        return false;
    }
    return true;
}

bool IsRelayPort(int port) {
    return port == kRelayPorts[0] || port == kRelayPorts[1];
}

} // namespace

bool ParseArgs(int argc, char** argv, Config& cfg, int& exitCode) {
    exitCode = 0;
    for (int i = 1; i < argc; ++i) {
        const char* a = argv[i];
        if (!std::strcmp(a, "--help") || !std::strcmp(a, "-h")) {
            PrintUsage(argv[0]); return false;
        } else if (!std::strcmp(a, "--serial")) {
            if (!NeedArg(i, argc, a)) { exitCode = 2; return false; }
            char* end = nullptr;
            errno = 0;
            long n = std::strtol(argv[++i], &end, 10);
            if (end == argv[i] || *end != '\0' || errno != 0 || n == 0 || n < -1 ||
                n > 0x7fffffffL) {
                std::fprintf(stderr, "ERROR: bad --serial '%s' (want a serial number or -1)\n", argv[i]);
                exitCode = 2; return false;
            }
            cfg.serial = static_cast<int>(n);
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
            if (end == argv[i] || *end != '\0' || !(d >= 0.0 && d <= 1.0)) {
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
        } else if (!std::strcmp(a, "--socket")) {
            if (!NeedArg(i, argc, a)) { exitCode = 2; return false; }
            const char* p = argv[++i];
            if (p[0] != '/' || std::strlen(p) >= sizeof(sockaddr_un{}.sun_path)) {
                std::fprintf(stderr, "ERROR: bad --socket '%s' (want an absolute path < %zu bytes)\n",
                             p, sizeof(sockaddr_un{}.sun_path));
                exitCode = 2; return false;
            }
            cfg.socketPath = p;
        } else {
            std::fprintf(stderr, "ERROR: unknown argument '%s' (try --help)\n", a);
            exitCode = 2; return false;
        }
    }
    if (IsRelayPort(cfg.fan.hubPort) || IsRelayPort(cfg.ssr.hubPort)) {
        std::fprintf(stderr, "ERROR: hub ports %d and %d are the relays; pick another --fan/--ssr port\n",
                     kRelayPorts[0], kRelayPorts[1]);
        exitCode = 2; return false;
    }
    if (cfg.fan.hubPort == cfg.ssr.hubPort) {
        std::fprintf(stderr, "ERROR: --fan and --ssr can't share hub port %d\n", cfg.fan.hubPort);
        exitCode = 2; return false;
    }
    return true;
}

// =============================================================================
// Protocol parser
// =============================================================================
namespace {

std::vector<std::string> SplitSpaces(const std::string& s, bool& ok) {
    // Exactly one ASCII space between tokens; no leading/trailing space.
    std::vector<std::string> out;
    ok = true;
    size_t start = 0;
    for (;;) {
        size_t sp = s.find(' ', start);
        std::string tok = s.substr(start, sp == std::string::npos ? std::string::npos : sp - start);
        if (tok.empty()) { ok = false; return out; }
        out.push_back(tok);
        if (sp == std::string::npos) return out;
        start = sp + 1;
    }
}

// One ASCII digit 0..max-1, nothing else (no sign, no leading zero).
bool ParseCh(const std::string& tok, int max, int& ch) {
    if (tok.size() != 1 || tok[0] < '0' || tok[0] > '9') return false;
    ch = tok[0] - '0';
    return ch < max;
}

// Plain decimal: digits with at most one '.', at least one digit, value 0..1.
bool ParseDuty(const std::string& tok, double& d) {
    if (tok.empty() || tok.size() > 16) return false;
    int digits = 0, dots = 0;
    for (char c : tok) {
        if (c >= '0' && c <= '9') ++digits;
        else if (c == '.') ++dots;
        else return false;
    }
    if (digits == 0 || dots > 1) return false;
    char* end = nullptr;
    d = std::strtod(tok.c_str(), &end);
    if (end != tok.c_str() + tok.size()) return false;
    return d >= 0.0 && d <= 1.0;
}

} // namespace

bool ParseRequest(const std::string& line, Request& out, std::string& err) {
    if (line.empty()) { err = "empty request"; return false; }
    if (line.size() > kMaxRequest) { err = "line too long"; return false; }
    bool ok = false;
    std::vector<std::string> t = SplitSpaces(line, ok);
    if (!ok) { err = "malformed request"; return false; }
    const std::string& verb = t[0];

    if (verb == "open" || verb == "close") {
        if (t.size() != 2) { err = "usage: " + verb + " <ch 0-7>"; return false; }
        if (!ParseCh(t[1], kRelayCount, out.ch)) { err = "bad relay channel"; return false; }
        out.type = verb == "open" ? ReqType::Open : ReqType::Close;
        return true;
    }
    if (verb == "fan") {
        if (t.size() != 3) { err = "usage: fan <ch 0-3> <duty 0.0-1.0>"; return false; }
        if (!ParseCh(t[1], 4, out.ch)) { err = "bad fan channel"; return false; }
        if (!ParseDuty(t[2], out.duty)) { err = "bad duty"; return false; }
        out.type = ReqType::Fan;
        return true;
    }
    if (verb == "ssr") {
        if (t.size() != 3) { err = "usage: ssr <ch 0-3> on|off"; return false; }
        if (!ParseCh(t[1], 4, out.ch)) { err = "bad ssr channel"; return false; }
        if (t[2] == "on") out.on = true;
        else if (t[2] == "off") out.on = false;
        else { err = "bad ssr value"; return false; }
        out.type = ReqType::Ssr;
        return true;
    }
    if (verb == "state") {
        if (t.size() != 1) { err = "usage: state"; return false; }
        out.type = ReqType::State;
        return true;
    }
    err = "unknown command";
    return false;
}

// =============================================================================
// Controller
// =============================================================================
Controller::Controller(Hal& hal, const Config& cfg, std::atomic<bool>& running)
    : hal_(hal), cfg_(cfg), running_(running),
      resetInterval_(cfg.failsafeMs / 3) {
    fatal_ = [] { std::exit(1); };
    sleep_ = [this](std::chrono::milliseconds total) {
        const auto deadline = std::chrono::steady_clock::now() + total;
        while (std::chrono::steady_clock::now() < deadline && running_.load())
            std::this_thread::sleep_for(std::chrono::milliseconds(50));
    };

    // Relays first so relay ch N is table index N (staggered power-on order).
    for (int i = 0; i < kRelayCount; ++i) {
        Channel c;
        c.kind = Kind::Relay;
        c.number = i;
        c.hubPort = kRelayPorts[i / 4];
        c.channel = i % 4;
        c.cmdOn = true;   // all relays default closed (load on)
        ch_.push_back(c);
    }
    for (int n : cfg.fan.channels) {
        Channel c;
        c.kind = Kind::FanPwm;
        c.number = n;
        c.hubPort = cfg.fan.hubPort;
        c.channel = n;
        c.duty = cfg.duty;
        ch_.push_back(c);
    }
    for (int n : cfg.ssr.channels) {
        Channel c;
        c.kind = Kind::Ssr;
        c.number = n;
        c.hubPort = cfg.ssr.hubPort;
        c.channel = n;
        c.cmdOn = true;   // SSR on by default
        ch_.push_back(c);
    }
}

std::string Controller::Describe(int idx) const {
    const Channel& c = ch_[idx];
    char buf[64];
    std::snprintf(buf, sizeof buf, "%s %d (port %d ch %d)",
                  KindWord(c.kind), c.number, c.hubPort, c.channel);
    return buf;
}

int Controller::Find(Kind k, int number) const {
    for (int i = 0; i < Size(); ++i)
        if (ch_[i].kind == k && ch_[i].number == number) return i;
    return -1;
}

Channel Controller::Snapshot(int idx) const {
    std::lock_guard<std::mutex> lk(mu_);
    return ch_[idx];
}

void Controller::Check(Rc rc, const char* what) {
    if (rc != kOk) {
        std::fprintf(stderr, "FATAL: %s failed: %s (%d)\n", what, hal_.ErrStr(rc), rc);
        std::fflush(stderr);
        fatal_();
        std::exit(1);   // in case a FatalFn returns
    }
}

// ---- Startup -----------------------------------------------------------------
// Locking: cmdMu_ serializes everything that changes an output (socket
// commands, the stagger, attach-time driving). mu_ guards the channel table and
// is NEVER held across a HAL call, so a slow device round trip can't stall the
// heartbeat. Lock order: cmdMu_, then mu_.
void Controller::CreateAll() {
    for (int i = 0; i < Size(); ++i) {
        const char* what = "create";
        // Be specific: serial + hub port + channel.
        Check(hal_.Create(i, cfg_.serial, ch_[i].hubPort, ch_[i].channel, this, &what), what);
    }
}

void Controller::OpenRelaysBlocking() {
    // relay_controller, verbatim: open every relay, waiting for attachment,
    // start FALSE. Do NOT arm the failsafe here: a channel's failsafe timer
    // starts the instant it's enabled, so arming all 8 up front means relays
    // that haven't reached their turn in the stagger (stagger delay > failsafe
    // time) would time out before being fed. Each is armed as it turns on.
    // No lock is held across openWaitForAttachment: the attach handler locks.
    int n = 0;
    for (int i = 0; i < Size(); ++i) {
        if (ch_[i].kind != Kind::Relay) continue;
        if (!running_.load()) {   // SIGTERM during a slow startup: stop promptly
            std::fprintf(stderr, "Stop requested during startup; not opening the remaining relays\n");
            break;
        }
        Check(hal_.OpenWaitForAttachment(i, kAttachTimeoutMs), "openWaitForAttachment");
        std::lock_guard<std::mutex> ck(cmdMu_);
        Check(hal_.SetState(i, false), "setState(off)");  // start open
        ++n;
    }
    std::fprintf(stderr, "All %d channels open (failsafe %ums, armed as each turns on)\n",
                 n, cfg_.failsafeMs);
}

void Controller::OpenDynamic() {
    // fan_control's dynamic model: non-blocking open; each channel is driven
    // (and its failsafe armed) in OnAttach, and re-driven on re-attach.
    int n = 0;
    for (int i = 0; i < Size(); ++i) {
        if (ch_[i].kind == Kind::Relay) continue;
        Rc rc = hal_.Open(i);
        if (rc != kOk) {
            std::fprintf(stderr, "FATAL: open failed: %s (%d)\n", hal_.ErrStr(rc), rc);
            std::fflush(stderr);
            fatal_();
            std::exit(1);
        }
        ++n;
    }
    std::fprintf(stderr,
        "Started (dynamic). %d channels opened; fans @ %.0f%%, SSR on as they "
        "attach. Failsafe %ums.\n", n, cfg_.duty * 100.0, cfg_.failsafeMs);
}

void Controller::RunStagger() {
    // relay_controller's one-time staggered power-on: relay i gets slot i
    // (index-based, kStaggerDelay apart). For each: close the relay, then arm
    // its failsafe; from then on the heartbeat feeds it. Added: a relay that is
    // commanded `open` (or lost: detached/tripped) before its slot is skipped,
    // and one already switched on by `close` is left alone. A skipped relay
    // keeps its (empty) slot so the others' timing doesn't depend on commands.
    for (int i = 0; i < Size() && running_.load(); ++i) {
        if (ch_[i].kind != Kind::Relay) continue;
        {
            std::lock_guard<std::mutex> ck(cmdMu_);
            Channel c;
            { std::lock_guard<std::mutex> lk(mu_); c = ch_[i]; }
            if (!c.attached || c.tripped) {
                std::fprintf(stderr, "Channel %d (port %d ch %d) skipped: %s\n",
                             i, c.hubPort, c.channel, c.tripped ? "failsafe tripped" : "not attached");
            } else if (!c.cmdOn) {
                std::fprintf(stderr, "Channel %d (port %d ch %d) skipped: commanded open\n",
                             i, c.hubPort, c.channel);
            } else if (c.armed) {
                std::fprintf(stderr, "Channel %d (port %d ch %d) already ON (commanded close)\n",
                             i, c.hubPort, c.channel);
            } else {
                Check(hal_.SetState(i, true), "setState(on)");                  // close -> load on
                Check(hal_.EnableFailsafe(i, cfg_.failsafeMs), "enableFailsafe");
                std::lock_guard<std::mutex> lk(mu_);
                if (ch_[i].epoch == c.epoch) {   // not detached/tripped meanwhile
                    ch_[i].armed = true;
                    ch_[i].failStreak = 0;
                }
                std::fprintf(stderr, "Channel %d (port %d ch %d) ON\n", i, c.hubPort, c.channel);
            }
        }
        sleep_(kStaggerDelay);
    }
    std::fprintf(stderr, "Stagger done\n");
}

// ---- Heartbeat ---------------------------------------------------------------
void Controller::HeartbeatTick() {
    std::vector<int> todo;
    {
        std::lock_guard<std::mutex> lk(mu_);
        for (int i = 0; i < Size(); ++i) {
            const Channel& c = ch_[i];
            if (c.armed && c.attached && !c.tripped) todo.push_back(i);
        }
    }
    std::vector<Rc> rcs;
    rcs.reserve(todo.size());
    for (int i : todo) rcs.push_back(hal_.ResetFailsafe(i));   // no lock held
    std::lock_guard<std::mutex> lk(mu_);
    for (size_t k = 0; k < todo.size(); ++k) {
        const int i = todo[k];
        Channel& c = ch_[i];
        if (rcs[k] == kOk) {
            c.failStreak = 0;
            continue;
        }
        if (++c.failStreak == kResetFailWarn)
            std::fprintf(stderr, "WARN: %s failsafe reset failing: %s (%d)\n",
                         Describe(i).c_str(), hal_.ErrStr(rcs[k]), rcs[k]);
        if (c.armed) NoteFailureLocked(i, rcs[k]);
    }
}

void Controller::HeartbeatLoop() {
    // Hold everything by resetting the failsafe faster than it expires (every
    // 1/3 of the window). Stop resetting (crash/kill/hang) and the hardware
    // drives every armed output FALSE: relays open, fans 0, SSR off.
    while (running_.load()) {
        HeartbeatTick();
        std::this_thread::sleep_for(resetInterval_);
    }
}

// ---- Failsafe-trip recovery ------------------------------------------------------
void Controller::MaintenanceTick() {
    // A tripped channel rejects input until reopened. Reopen it (close + open,
    // non-blocking); OnAttach then clears the trip. Fans/SSR are re-driven to
    // their commanded value; a relay comes back open and unarmed (as after a
    // VINT blip) until `close <ch>`. Retried every kReopenRetry. No lock is
    // held across Reopen: close/open can fire the detach/attach handlers.
    const auto now = std::chrono::steady_clock::now();
    std::vector<int> todo;
    {
        std::lock_guard<std::mutex> lk(mu_);
        for (int i = 0; i < Size(); ++i) {
            Channel& c = ch_[i];
            if (!c.tripped) continue;
            if (c.reopened && now - c.lastReopen < kReopenRetry) continue;
            c.reopened = true;
            c.lastReopen = now;
            todo.push_back(i);
        }
    }
    for (int i : todo) {
        std::fprintf(stderr, "Reopening %s after failsafe trip\n", Describe(i).c_str());
        Rc rc = hal_.Reopen(i);
        if (rc != kOk)
            std::fprintf(stderr, "ERROR: reopen %s failed: %s (%d); will retry\n",
                         Describe(i).c_str(), hal_.ErrStr(rc), rc);
    }
}

// ---- Shutdown ----------------------------------------------------------------
void Controller::Shutdown() {
    std::fprintf(stderr, "Shutting down: relays open, fans 0, SSR off\n");
    {
        std::lock_guard<std::mutex> ck(cmdMu_);
        for (int i = 0; i < Size(); ++i) {
            if (ch_[i].kind == Kind::FanPwm) hal_.SetDutyCycle(i, 0.0);
            else                             hal_.SetState(i, false);
        }
        std::lock_guard<std::mutex> lk(mu_);
        for (Channel& c : ch_) c.armed = false;
    }
    // Close without any lock: closing can fire the detach handler.
    for (int i = 0; i < Size(); ++i) hal_.Close(i);
}

// ---- Events --------------------------------------------------------------------
void Controller::OnAttach(int idx) {
    if (idx < 0 || idx >= Size()) return;
    // cmdMu_ (not just mu_): the attach-time drive must not interleave with a
    // socket command on the same channel.
    std::lock_guard<std::mutex> ck(cmdMu_);
    uint64_t ep;
    {
        std::lock_guard<std::mutex> lk(mu_);
        Channel& c = ch_[idx];
        c.attached = true;
        c.tripped = false;   // a (re-)attach is a freshly opened channel
        c.reopened = false;
        ep = ++c.epoch;
        std::fprintf(stderr, "ATTACH: %s\n", Describe(idx).c_str());
    }
    // Relays (relay_controller, verbatim): log only. A re-attached relay
    // comes back open and unarmed (reported cmd=open); `close <ch>` re-drives it.
    if (ch_[idx].kind != Kind::Relay) Drive(idx, ep);
}

void Controller::OnDetach(int idx) {
    std::lock_guard<std::mutex> lk(mu_);
    if (idx < 0 || idx >= Size()) return;
    Channel& c = ch_[idx];
    c.attached = false;
    c.armed = false;   // a detached device is unpowered on VINT: output off, failsafe gone
    ++c.epoch;
    if (c.kind == Kind::Relay) c.cmdOn = false;   // physically open now; report it
    std::fprintf(stderr, "DETACH: %s -- lost from VINT\n", Describe(idx).c_str());
}

void Controller::OnError(int idx, int code, const char* description, bool failsafeTrip) {
    std::lock_guard<std::mutex> lk(mu_);
    if (idx < 0 || idx >= Size()) return;
    std::fprintf(stderr, "ERROR: %s: %s (%d)\n", Describe(idx).c_str(),
                 description ? description : "?", code);
    if (failsafeTrip) MarkTrippedLocked(idx);
}

void Controller::MarkTrippedLocked(int idx) {
    Channel& c = ch_[idx];
    if (c.tripped) return;
    c.tripped = true;
    c.armed = false;
    ++c.epoch;
    if (c.kind == Kind::Relay) c.cmdOn = false;   // the failsafe forced it open
    std::fprintf(stderr, "ERROR: %s failsafe TRIPPED: output forced off; rejecting input "
                 "until the channel is reopened\n", Describe(idx).c_str());
}

void Controller::NoteFailureLocked(int idx, Rc rc) {
    if (hal_.IsFailsafeRc(rc)) MarkTrippedLocked(idx);
}

std::string Controller::FailLocked(int idx, Rc rc, const char* what) {
    std::fprintf(stderr, "ERROR: %s %s failed: %s (%d)\n",
                 Describe(idx).c_str(), what, hal_.ErrStr(rc), rc);
    NoteFailureLocked(idx, rc);
    return Name(idx) + " " + what + " failed: " + hal_.ErrStr(rc);
}

std::string Controller::Name(int idx) const {
    return std::string(KindWord(ch_[idx].kind)) + " " + std::to_string(ch_[idx].number);
}

// fan_control's DriveChannel: set the output to its commanded value, then arm
// the failsafe. Called with cmdMu_ held (so the commanded value is stable)
// and mu_ NOT held.
bool Controller::Drive(int idx, uint64_t ep) {
    Channel c;
    { std::lock_guard<std::mutex> lk(mu_); c = ch_[idx]; }
    Rc rc = c.kind == Kind::FanPwm ? hal_.SetDutyCycle(idx, c.duty)
                                   : hal_.SetState(idx, c.cmdOn);
    if (rc != kOk) {
        std::lock_guard<std::mutex> lk(mu_);
        FailLocked(idx, rc, "set-output");
        return false;
    }
    rc = hal_.EnableFailsafe(idx, cfg_.failsafeMs);
    std::lock_guard<std::mutex> lk(mu_);
    if (rc != kOk) {
        FailLocked(idx, rc, "enableFailsafe");
        return false;
    }
    if (ch_[idx].epoch != ep) return false;   // detached/tripped meanwhile
    ch_[idx].armed = true;
    ch_[idx].failStreak = 0;
    if (c.kind == Kind::FanPwm)
        std::fprintf(stderr, "%s -> %.0f%%\n", Describe(idx).c_str(), c.duty * 100.0);
    else
        std::fprintf(stderr, "%s -> %s\n", Describe(idx).c_str(), c.cmdOn ? "ON" : "OFF");
    return true;
}

// ---- Socket commands ---------------------------------------------------------
bool Controller::UsableLocked(int idx, std::string& err) const {
    const Channel& c = ch_[idx];
    if (c.tripped) { err = Name(idx) + " failsafe tripped"; return false; }
    if (!c.attached) { err = Name(idx) + " not attached"; return false; }
    return true;
}

// Apply one command to channel idx: arm the failsafe first if `arm` and it
// isn't armed yet (output still unchanged, so a channel is never left on
// without a failsafe), then `set`, then record the new commanded value under
// mu_. cmdMu_ is held by the caller; mu_ is not.
bool Controller::Apply(int idx, bool arm, const std::function<Rc()>& set, const char* what,
                       const std::function<void(Channel&)>& record, std::string& err) {
    uint64_t ep;
    bool armed;
    {
        std::lock_guard<std::mutex> lk(mu_);
        if (!UsableLocked(idx, err)) return false;
        ep = ch_[idx].epoch;
        armed = ch_[idx].armed;
    }
    if (arm && !armed) {
        Rc rc = hal_.EnableFailsafe(idx, cfg_.failsafeMs);
        std::lock_guard<std::mutex> lk(mu_);
        if (rc != kOk) { err = FailLocked(idx, rc, "enableFailsafe"); return false; }
        if (ch_[idx].epoch != ep) { err = Name(idx) + " detached or tripped during command"; return false; }
        ch_[idx].armed = true;
        ch_[idx].failStreak = 0;
    }
    Rc rc = set();
    std::lock_guard<std::mutex> lk(mu_);
    if (rc != kOk) { err = FailLocked(idx, rc, what); return false; }
    if (ch_[idx].epoch != ep) { err = Name(idx) + " detached or tripped during command"; return false; }
    record(ch_[idx]);
    return true;
}

std::vector<std::string> Controller::Handle(const std::string& line) {
    Request req;
    std::string err;
    if (!ParseRequest(line, req, err)) return {"err " + err};

    if (req.type != ReqType::State) {
        std::lock_guard<std::mutex> ck(cmdMu_);
        switch (req.type) {
            case ReqType::Open:
            case ReqType::Close: {
                const bool on = req.type == ReqType::Close;
                const int idx = RelayIndex(req.ch);
                if (!Apply(idx, on, [&] { return hal_.SetState(idx, on); }, "setState",
                           [on](Channel& c) { c.cmdOn = on; }, err))
                    return {"err " + err};
                std::fprintf(stderr, "CMD: %s %s\n", Describe(idx).c_str(), on ? "close" : "open");
                return {std::string("ok ") + (on ? "close " : "open ") + std::to_string(req.ch)};
            }
            case ReqType::Ssr: {
                const int idx = Find(Kind::Ssr, req.ch);
                if (idx < 0) return {"err ssr " + std::to_string(req.ch) + " not configured"};
                const bool on = req.on;
                if (!Apply(idx, on, [&] { return hal_.SetState(idx, on); }, "setState",
                           [on](Channel& c) { c.cmdOn = on; }, err))
                    return {"err " + err};
                std::fprintf(stderr, "CMD: %s %s\n", Describe(idx).c_str(), on ? "on" : "off");
                return {"ok ssr " + std::to_string(req.ch) + (on ? " on" : " off")};
            }
            case ReqType::Fan: {
                const int idx = Find(Kind::FanPwm, req.ch);
                if (idx < 0) return {"err fan " + std::to_string(req.ch) + " not configured"};
                const double d = req.duty;
                if (!Apply(idx, true, [&] { return hal_.SetDutyCycle(idx, d); }, "setDutyCycle",
                           [d](Channel& c) { c.duty = d; }, err))
                    return {"err " + err};
                char buf[48];
                std::snprintf(buf, sizeof buf, "ok fan %d %.2f", req.ch, d);
                std::fprintf(stderr, "CMD: %s -> %.0f%%\n", Describe(idx).c_str(), d * 100.0);
                return {buf};
            }
            case ReqType::State:
                break;
        }
        return {"err internal error"};
    }

    // state: a consistent snapshot under mu_ alone.
    std::lock_guard<std::mutex> lk(mu_);
    std::vector<std::string> out;
    char buf[64];
    for (const Channel& c : ch_) {
        const int att = c.attached && !c.tripped ? 1 : 0;
        switch (c.kind) {
            case Kind::Relay:
                std::snprintf(buf, sizeof buf, "relay %d cmd=%s attached=%d",
                              c.number, c.cmdOn ? "closed" : "open", att);
                break;
            case Kind::FanPwm:
                std::snprintf(buf, sizeof buf, "fan %d duty=%.2f attached=%d",
                              c.number, c.duty, att);
                break;
            case Kind::Ssr:
                std::snprintf(buf, sizeof buf, "ssr %d cmd=%s attached=%d",
                              c.number, c.cmdOn ? "on" : "off", att);
                break;
        }
        out.emplace_back(buf);
    }
    out.emplace_back("end");
    return out;
}

// =============================================================================
// Unix-socket server
// =============================================================================
namespace {

constexpr size_t kReadChunk   = 512;
constexpr size_t kOutPause    = 4096;    // stop reading a client with this much unsent
constexpr size_t kOutMax      = 65536;   // drop a client that won't read its replies
constexpr int    kPollMs      = 200;
constexpr int    kBindRetryMs = 2000;

#ifdef MSG_NOSIGNAL
constexpr int kSendFlags = MSG_NOSIGNAL;
#else
constexpr int kSendFlags = 0;            // macOS: SO_NOSIGPIPE below
#endif

struct Client {
    int fd = -1;
    std::string in;
    std::string out;
    bool discarding = false;   // inside an over-long line; skip to '\n'
    bool eof = false;          // peer closed its write side
    bool dead = false;
};

bool SetNonBlocking(int fd) {
    int fl = fcntl(fd, F_GETFL, 0);
    if (fl < 0 || fcntl(fd, F_SETFL, fl | O_NONBLOCK) < 0) return false;
    fcntl(fd, F_SETFD, FD_CLOEXEC);
    return true;
}

void NoSigPipe(int fd) {
#ifdef SO_NOSIGPIPE
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
#else
    (void)fd;
#endif
}

void FlushClient(Client& c) {
    while (!c.out.empty()) {
        ssize_t n = send(c.fd, c.out.data(), c.out.size(), kSendFlags);
        if (n > 0) { c.out.erase(0, static_cast<size_t>(n)); continue; }
        if (n < 0 && errno == EINTR) continue;
        if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return;
        c.dead = true;   // EPIPE / ECONNRESET / ...
        return;
    }
}

void HandleLine(Controller& ctl, Client& c, const std::string& line) {
    std::vector<std::string> reply;
    try {
        reply = ctl.Handle(line);
    } catch (const std::exception& e) {
        std::fprintf(stderr, "ERROR: request handling threw: %s\n", e.what());
        reply = {"err internal error"};
    } catch (...) {
        reply = {"err internal error"};
    }
    for (const std::string& l : reply) { c.out += l; c.out += '\n'; }
}

void ConsumeInput(Controller& ctl, Client& c, const char* data, size_t n) {
    for (size_t i = 0; i < n; ++i) {
        const char b = data[i];
        if (c.discarding) {
            if (b == '\n') c.discarding = false;
            continue;
        }
        if (b == '\n') {
            HandleLine(ctl, c, c.in);
            c.in.clear();
            continue;
        }
        c.in.push_back(b);
        if (c.in.size() > kMaxRequest) {
            c.out += "err line too long\n";
            c.in.clear();
            c.discarding = true;
        }
    }
}

} // namespace

SocketServer::SocketServer(Controller& ctl, std::string path, std::atomic<bool>& running)
    : ctl_(ctl), path_(std::move(path)), running_(running) {}

bool SocketServer::Bind() {
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    if (path_.empty() || path_.size() >= sizeof addr.sun_path) {
        std::fprintf(stderr, "ERROR: socket path '%s' unusable\n", path_.c_str());
        return false;
    }
    std::memcpy(addr.sun_path, path_.c_str(), path_.size() + 1);

    // Replace a stale socket from a previous run, but never another file type.
    struct stat st{};
    if (lstat(path_.c_str(), &st) == 0) {
        if (!S_ISSOCK(st.st_mode)) {
            std::fprintf(stderr, "ERROR: %s exists and is not a socket; not touching it\n",
                         path_.c_str());
            return false;
        }
        if (unlink(path_.c_str()) != 0) {
            std::fprintf(stderr, "ERROR: unlink stale %s: %s\n", path_.c_str(), std::strerror(errno));
            return false;
        }
    }

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        std::fprintf(stderr, "ERROR: socket(): %s\n", std::strerror(errno));
        return false;
    }
    if (bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof addr) != 0) {
        std::fprintf(stderr, "ERROR: bind %s: %s\n", path_.c_str(), std::strerror(errno));
        close(fd);
        return false;
    }
    // 0660 regardless of umask (the unit's UMask also gives 0660). The group
    // is the service's Group=rackctl, so relay-mqtt can connect.
    if (chmod(path_.c_str(), 0660) != 0 || listen(fd, 16) != 0 || !SetNonBlocking(fd)) {
        std::fprintf(stderr, "ERROR: setting up %s: %s\n", path_.c_str(), std::strerror(errno));
        close(fd);
        unlink(path_.c_str());
        return false;
    }
    listenFd_ = fd;
    std::fprintf(stderr, "Control socket listening on %s\n", path_.c_str());
    return true;
}

void SocketServer::Run() {
    std::vector<Client> clients;
    auto lastBindTry = std::chrono::steady_clock::now() - std::chrono::hours(1);

    while (running_.load()) {
        if (listenFd_ < 0) {
            const auto now = std::chrono::steady_clock::now();
            if (now - lastBindTry >= std::chrono::milliseconds(kBindRetryMs)) {
                lastBindTry = now;
                if (!Bind()) std::fprintf(stderr, "WARN: control socket not up; retrying\n");
            }
        }

        std::vector<pollfd> fds;
        if (listenFd_ >= 0) fds.push_back(pollfd{listenFd_, POLLIN, 0});
        const size_t base = fds.size();
        for (Client& c : clients) {
            short ev = 0;
            if (!c.eof && c.out.size() < kOutPause) ev |= POLLIN;
            if (!c.out.empty()) ev |= POLLOUT;
            fds.push_back(pollfd{c.fd, ev, 0});
        }

        int pr = poll(fds.data(), static_cast<nfds_t>(fds.size()), kPollMs);
        if (pr < 0) {
            if (errno != EINTR)
                std::fprintf(stderr, "WARN: poll: %s\n", std::strerror(errno));
            continue;
        }

        for (size_t i = 0; i < clients.size(); ++i) {
            Client& c = clients[i];
            const short re = fds[base + i].revents;
            if (re & (POLLIN | POLLHUP | POLLERR)) {
                char buf[kReadChunk];
                ssize_t n = recv(c.fd, buf, sizeof buf, 0);
                if (n > 0) {
                    ConsumeInput(ctl_, c, buf, static_cast<size_t>(n));
                } else if (n == 0) {
                    c.eof = true;   // flush pending replies, then close
                } else if (errno != EINTR && errno != EAGAIN && errno != EWOULDBLOCK) {
                    c.dead = true;
                }
            }
            if (!c.dead && !c.out.empty()) FlushClient(c);
            if (c.out.size() > kOutMax) {
                std::fprintf(stderr, "WARN: control client not reading replies; dropping it\n");
                c.dead = true;
            }
            if (c.eof && c.out.empty()) c.dead = true;
        }

        // Accept after servicing, so `clients` isn't resized under fds.
        if (base == 1 && (fds[0].revents & POLLIN)) {
            for (;;) {
                int cfd = accept(listenFd_, nullptr, nullptr);
                if (cfd < 0) {
                    if (errno == EINTR) continue;
                    if (errno != EAGAIN && errno != EWOULDBLOCK)
                        std::fprintf(stderr, "WARN: accept: %s\n", std::strerror(errno));
                    break;
                }
                NoSigPipe(cfd);
                if (!SetNonBlocking(cfd)) { close(cfd); continue; }
                if (clients.size() >= kMaxClients) {
                    static const char kBusy[] = "err too many clients\n";
                    (void)!send(cfd, kBusy, sizeof kBusy - 1, kSendFlags);
                    close(cfd);
                    continue;
                }
                Client c;
                c.fd = cfd;
                clients.push_back(std::move(c));
            }
        }

        for (size_t i = 0; i < clients.size();) {
            if (clients[i].dead) {
                close(clients[i].fd);
                clients.erase(clients.begin() + static_cast<long>(i));
            } else {
                ++i;
            }
        }
    }

    for (Client& c : clients) close(c.fd);
    if (listenFd_ >= 0) {
        close(listenFd_);
        listenFd_ = -1;
        unlink(path_.c_str());
    }
}

} // namespace rc
