// Unit tests for rack_controller's core: protocol parser, commanded-state
// logic, startup/stagger/heartbeat against a fake HAL, CLI parsing, and the
// socket server over a real AF_UNIX socket in a temp dir.

#include "../core.h"
#include "fake_hal.h"

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

using namespace rc;

// ---- Tiny test harness ---------------------------------------------------------
static int g_fail = 0, g_checks = 0;
#define CHECK(cond) do { ++g_checks; if (!(cond)) { ++g_fail; \
    std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); } } while (0)
#define CHECK_EQ(a, b) do { ++g_checks; auto va_ = (a); auto vb_ = (b); if (!(va_ == vb_)) { ++g_fail; \
    std::fprintf(stderr, "FAIL %s:%d: %s == %s\n  got:  %s\n  want: %s\n", __FILE__, __LINE__, \
                 #a, #b, Str(va_).c_str(), Str(vb_).c_str()); } } while (0)

static std::string Str(const std::string& s) { return "\"" + s + "\""; }
[[maybe_unused]] static std::string Str(const char* s) { return Str(std::string(s)); }
static std::string Str(bool b) { return b ? "true" : "false"; }
static std::string Str(int v) { return std::to_string(v); }
[[maybe_unused]] static std::string Str(long v) { return std::to_string(v); }
static std::string Str(unsigned v) { return std::to_string(v); }
static std::string Str(unsigned long v) { return std::to_string(v); }
static std::string Str(double v) { return std::to_string(v); }
static std::string Str(const std::vector<int>& v) {
    std::string s = "[";
    for (int x : v) s += std::to_string(x) + ", ";
    return s + "]";
}
static std::string Str(const std::vector<std::string>& v) {
    std::string s = "[";
    for (auto& x : v) s += Str(x) + ", ";
    return s + "]";
}

struct Fatal : std::runtime_error { Fatal() : std::runtime_error("fatal") {} };

// Index layout with the default config: relays 0-7, fans 8-11, SSR 12-15.
constexpr int kFan0 = 8, kSsr0 = 12;

struct Rig {
    FakeHal hal;
    Config cfg;
    std::atomic<bool> running{true};
    std::unique_ptr<Controller> ctl;
    std::vector<std::chrono::milliseconds> sleeps;
    std::function<void(int)> onSlot;   // called with the slot number during the stagger

    explicit Rig(Config c = Config()) : cfg(std::move(c)) {
        ctl.reset(new Controller(hal, cfg, running));
        ctl->SetFatal([] { throw Fatal(); });
        ctl->SetSleep([this](std::chrono::milliseconds d) {
            const int slot = static_cast<int>(sleeps.size());
            sleeps.push_back(d);
            if (onSlot) onSlot(slot);
        });
    }
    // Full startup, as main() does it (without threads).
    void Start() {
        ctl->CreateAll();
        ctl->OpenRelaysBlocking();
        ctl->OpenDynamic();
        for (int i = kFan0; i < ctl->Size(); ++i) hal.FireAttach(i);
        ctl->RunStagger();
    }
    std::string One(const std::string& line) {
        auto r = ctl->Handle(line);
        return r.size() == 1 ? r[0] : "<" + std::to_string(r.size()) + " lines>";
    }
};

static bool StartsWith(const std::string& s, const char* p) { return s.rfind(p, 0) == 0; }

// ---- Parser --------------------------------------------------------------------
static void TestParserValid() {
    Request r; std::string err;
    for (int ch = 0; ch < 8; ++ch) {
        CHECK(ParseRequest("open " + std::to_string(ch), r, err));
        CHECK(r.type == ReqType::Open); CHECK_EQ(r.ch, ch);
        CHECK(ParseRequest("close " + std::to_string(ch), r, err));
        CHECK(r.type == ReqType::Close); CHECK_EQ(r.ch, ch);
    }
    CHECK(ParseRequest("fan 3 0.25", r, err));
    CHECK(r.type == ReqType::Fan); CHECK_EQ(r.ch, 3); CHECK_EQ(r.duty, 0.25);
    for (const char* d : {"0", "1", "0.0", "1.0", "1.00", ".5", "0.123456"}) {
        CHECK(ParseRequest(std::string("fan 0 ") + d, r, err));
    }
    CHECK(ParseRequest("ssr 2 on", r, err));  CHECK(r.type == ReqType::Ssr); CHECK(r.on);
    CHECK(ParseRequest("ssr 0 off", r, err)); CHECK(!r.on);
    CHECK(ParseRequest("state", r, err));     CHECK(r.type == ReqType::State);
}

static void TestParserInvalid() {
    Request r; std::string err;
    const char* bad[] = {
        "", "foo", "OPEN 3", "Open 3", "open", "open 8", "open 9", "open -1", "open 03",
        "open +3", "open x", "open 3 4", " open 3", "open 3 ", "open  3", "open\t3",
        "open 3\r", "close 10", "close",
        "fan", "fan 0", "fan 4 0.5", "fan 0 1.5", "fan 0 -0.1", "fan 0 1.01", "fan 0 nan",
        "fan 0 inf", "fan 0 1e-1", "fan 0 0x1", "fan 0 .", "fan 0 0..5", "fan 0 0.5 x",
        "fan 0 abc", "fan 0 0.5.", "fan -1 0.5",
        "ssr", "ssr 0", "ssr 0 ON", "ssr 0 1", "ssr 4 on", "ssr 0 on x",
        "state x", "state ", "end", "ok open 3",
    };
    for (const char* b : bad) {
        err.clear();
        bool ok = ParseRequest(b, r, err);
        if (ok) std::fprintf(stderr, "  accepted bad request \"%s\"\n", b);
        CHECK(!ok);
        CHECK(!err.empty());
    }
    std::string nul("open 3");
    nul[4] = '\0';
    CHECK(!ParseRequest(nul, r, err));
    CHECK(!ParseRequest(std::string(129, 'a'), r, err));
    CHECK_EQ(err, std::string("line too long"));
}

// ---- Startup + stagger --------------------------------------------------------------
static void TestStartupAndStagger() {
    Rig t;
    t.ctl->CreateAll();
    t.ctl->OpenRelaysBlocking();
    for (int i = 0; i < 8; ++i) {
        auto o = t.hal.Get(i);
        CHECK(o.opened); CHECK(!o.state); CHECK(!o.failsafe);
        CHECK(t.ctl->Snapshot(i).attached);
        CHECK(!t.ctl->Snapshot(i).armed);
    }
    // Arming happens only as each relay turns on: at slot k, exactly relays 0..k are armed.
    t.onSlot = [&](int slot) {
        for (int i = 0; i < 8; ++i) {
            CHECK_EQ(t.hal.Get(i).failsafe, i <= slot);
            CHECK_EQ(t.hal.Get(i).state, i <= slot);
        }
    };
    t.ctl->RunStagger();
    CHECK_EQ(t.sleeps.size(), 8ul);
    for (auto d : t.sleeps) CHECK(d == std::chrono::milliseconds(2500));
    for (int i = 0; i < 8; ++i) {
        auto o = t.hal.Get(i);
        CHECK(o.state); CHECK(o.failsafe); CHECK_EQ(o.failsafeMs, 1000u); CHECK_EQ(o.enables, 1);
        CHECK(t.ctl->Snapshot(i).armed);
    }
    // Order: setState(1) then enableFailsafe, relay by relay, in index order.
    std::vector<std::string> want;
    for (int i = 0; i < 8; ++i) {
        want.push_back("setstate " + std::to_string(i) + " 1");
        want.push_back("enable " + std::to_string(i));
    }
    std::vector<std::string> got;
    for (auto& c : t.hal.Calls())
        if (StartsWith(c, "enable ") || (StartsWith(c, "setstate ") && c.back() == '1')) got.push_back(c);
    CHECK_EQ(got, want);
}

static void TestStaggerStopsOnSignal() {
    Rig t;
    t.ctl->CreateAll();
    t.ctl->OpenRelaysBlocking();
    t.onSlot = [&](int slot) { if (slot == 2) t.running = false; };
    t.ctl->RunStagger();
    CHECK(t.hal.Get(2).state);
    CHECK(!t.hal.Get(3).state);
}

static void TestOpenDuringStaggerSkipsRelay() {
    Rig t;
    t.ctl->CreateAll();
    t.ctl->OpenRelaysBlocking();
    t.onSlot = [&](int slot) {
        if (slot == 1) CHECK_EQ(t.One("open 5"), std::string("ok open 5"));
    };
    t.ctl->RunStagger();
    CHECK_EQ(t.sleeps.size(), 8ul);   // index-based: relay 5 keeps its (empty) slot
    CHECK(!t.hal.Get(5).state);
    CHECK(!t.hal.Get(5).failsafe);
    for (auto& c : t.hal.Calls()) CHECK(c != "setstate 5 1");
    for (int i = 0; i < 8; ++i) if (i != 5) CHECK(t.hal.Get(i).state);
    auto st = t.ctl->Handle("state");
    CHECK_EQ(st[5], std::string("relay 5 cmd=open attached=1"));
    CHECK_EQ(st[4], std::string("relay 4 cmd=closed attached=1"));

    // close arms the failsafe it never got, then the heartbeat feeds it.
    CHECK_EQ(t.One("close 5"), std::string("ok close 5"));
    auto o = t.hal.Get(5);
    CHECK(o.state); CHECK(o.failsafe); CHECK_EQ(o.enables, 1);
    CHECK(t.ctl->Snapshot(5).armed);
    int before = t.hal.Get(5).resets;
    t.ctl->HeartbeatTick();
    CHECK_EQ(t.hal.Get(5).resets, before + 1);
    // close again: no second enableFailsafe.
    CHECK_EQ(t.One("close 5"), std::string("ok close 5"));
    CHECK_EQ(t.hal.Get(5).enables, 1);
}

// --start-open: no relay is switched on or armed by the stagger; state says
// open; a later `close` arms then energizes just that relay.
static void TestStartOpenKeepsAllRelaysOff() {
    Config c;
    c.startOpen = true;
    Rig t(c);
    t.ctl->CreateAll();
    t.ctl->OpenRelaysBlocking();
    t.ctl->RunStagger();
    for (int i = 0; i < 8; ++i) {
        CHECK(!t.hal.Get(i).state);
        CHECK(!t.hal.Get(i).failsafe);
        CHECK(!t.ctl->Snapshot(i).armed);
    }
    for (auto& call : t.hal.Calls()) CHECK(!(StartsWith(call, "setstate ") && call.back() == '1'));
    auto st = t.ctl->Handle("state");
    for (int i = 0; i < 8; ++i)
        CHECK_EQ(st[i], "relay " + std::to_string(i) + " cmd=open attached=1");
    CHECK_EQ(t.One("close 2"), std::string("ok close 2"));
    CHECK(t.hal.Get(2).state); CHECK(t.hal.Get(2).failsafe);
    CHECK(!t.hal.Get(3).state);
}

static void TestStartOpenFlagParses() {
    const char* argv[] = {"rack_controller", "--start-open"};
    Config c; int code = 0;
    CHECK(ParseArgs(2, const_cast<char**>(argv), c, code));
    CHECK(c.startOpen);
    Config d;
    const char* argv2[] = {"rack_controller"};
    CHECK(ParseArgs(1, const_cast<char**>(argv2), d, code));
    CHECK(!d.startOpen);
}

static void TestCloseDuringStaggerArmsEarly() {
    Rig t;
    t.ctl->CreateAll();
    t.ctl->OpenRelaysBlocking();
    t.onSlot = [&](int slot) {
        if (slot == 0) {
            CHECK_EQ(t.One("close 6"), std::string("ok close 6"));
            CHECK(t.hal.Get(6).state);
            CHECK(t.hal.Get(6).failsafe);
        }
    };
    t.ctl->RunStagger();
    CHECK_EQ(t.hal.Get(6).enables, 1);   // the stagger left it alone
    // Arm-before-on for socket closes.
    auto calls = t.hal.Calls();
    size_t en = 0, on = 0;
    for (size_t i = 0; i < calls.size(); ++i) {
        if (calls[i] == "enable 6") en = i;
        if (calls[i] == "setstate 6 1") on = i;
    }
    CHECK(en < on);
}

// ---- Commands after startup ---------------------------------------------------------
static void TestRelayCommands() {
    Rig t;
    t.Start();
    CHECK_EQ(t.One("open 2"), std::string("ok open 2"));
    CHECK(!t.hal.Get(2).state);
    CHECK(t.ctl->Snapshot(2).armed);   // failsafe still fed while open
    int before = t.hal.Get(2).resets;
    t.ctl->HeartbeatTick();
    CHECK_EQ(t.hal.Get(2).resets, before + 1);
    CHECK_EQ(t.One("close 2"), std::string("ok close 2"));
    CHECK(t.hal.Get(2).state);
    CHECK_EQ(t.hal.Get(2).enables, 1);
    // open is idempotent.
    CHECK_EQ(t.One("open 7"), std::string("ok open 7"));
    CHECK_EQ(t.One("open 7"), std::string("ok open 7"));

    // Bad channels / payloads never touch the HAL.
    t.hal.ClearCalls();
    for (const char* b : {"open 8", "close -1", "open", "open 1 2", "OPEN 1", "close x"}) {
        CHECK(StartsWith(t.One(b), "err "));
    }
    CHECK(t.hal.Calls().empty());
}

static void TestSetStateFailureIsErrNotExit() {
    Rig t;
    t.Start();
    t.hal.failSetState.insert(3);
    std::string r = t.One("open 3");
    CHECK(StartsWith(r, "err relay 3 setState failed"));
    CHECK(t.ctl->Snapshot(3).cmdOn);          // commanded state unchanged
    CHECK(t.ctl->Snapshot(3).armed);
    CHECK_EQ(t.ctl->Handle("state")[3], std::string("relay 3 cmd=closed attached=1"));
    t.hal.failSetState.clear();
    CHECK_EQ(t.One("open 3"), std::string("ok open 3"));

    // enableFailsafe failure on close of an unarmed relay: output stays off.
    t.hal.failEnable.insert(3);
    t.hal.FireDetach(3);
    t.hal.FireAttach(3);
    std::string e = t.One("close 3");
    CHECK(StartsWith(e, "err relay 3 enableFailsafe failed"));
    CHECK(!t.hal.Get(3).state);
    CHECK(!t.ctl->Snapshot(3).cmdOn);
}

static void TestDetachAndReattach() {
    Rig t;
    t.Start();
    t.hal.FireDetach(1);
    // Reported as it really is: open (so relay-mqtt publishes `open`).
    CHECK_EQ(t.ctl->Handle("state")[1], std::string("relay 1 cmd=open attached=0"));
    CHECK(StartsWith(t.One("close 1"), "err relay 1 not attached"));
    int resets = t.hal.Get(1).resets;
    t.ctl->HeartbeatTick();
    CHECK_EQ(t.hal.Get(1).resets, resets);   // not fed while detached
    t.hal.FireAttach(1);
    // relay_controller behavior: a re-attached relay is not re-driven ...
    CHECK(!t.hal.Get(1).state);
    CHECK(!t.ctl->Snapshot(1).armed);
    CHECK_EQ(t.ctl->Handle("state")[1], std::string("relay 1 cmd=open attached=1"));
    // ... until commanded: close re-arms and closes it.
    CHECK_EQ(t.One("close 1"), std::string("ok close 1"));
    CHECK(t.hal.Get(1).state);
    CHECK(t.hal.Get(1).failsafe);
}

static void TestFailsafeTrip() {
    Rig t;
    t.Start();
    t.hal.FireTrip(4);
    CHECK(t.ctl->Snapshot(4).tripped);
    CHECK(StartsWith(t.One("open 4"), "err relay 4 failsafe tripped"));
    CHECK(StartsWith(t.One("close 4"), "err relay 4 failsafe tripped"));
    CHECK_EQ(t.ctl->Handle("state")[4], std::string("relay 4 cmd=open attached=0"));
    int resets = t.hal.Get(4).resets;
    t.ctl->HeartbeatTick();
    CHECK_EQ(t.hal.Get(4).resets, resets);   // no longer fed
    // A setState that reports the failsafe rc also marks the channel tripped.
    t.hal.failsafeSetState.insert(0);
    CHECK(StartsWith(t.One("open 0"), "err relay 0 setState failed"));
    CHECK(t.ctl->Snapshot(0).tripped);
    CHECK(StartsWith(t.One("open 0"), "err relay 0 failsafe tripped"));
    t.hal.failsafeSetState.clear();

    // Recovery: the maintenance tick reopens tripped channels once (retry after 5 s).
    t.hal.ClearCalls();
    t.ctl->MaintenanceTick();
    t.ctl->MaintenanceTick();
    CHECK_EQ(t.hal.Calls(), (std::vector<std::string>{"reopen 0", "reopen 4"}));
    t.hal.FireAttach(4);
    t.hal.FireAttach(0);
    CHECK(!t.ctl->Snapshot(4).tripped);
    CHECK(!t.hal.Get(4).state);   // a reopened relay stays open until commanded
    CHECK_EQ(t.ctl->Handle("state")[4], std::string("relay 4 cmd=open attached=1"));
    CHECK_EQ(t.One("close 4"), std::string("ok close 4"));
    CHECK(t.hal.Get(4).state);
    CHECK(t.hal.Get(4).failsafe);
    t.hal.ClearCalls();
    t.ctl->MaintenanceTick();
    CHECK(t.hal.Calls().empty());
}

static void TestFanTripRecovers() {
    Rig t;
    t.hal.attachOnOpen = true;   // Reopen re-attaches immediately
    t.Start();
    CHECK_EQ(t.One("fan 1 0.8"), std::string("ok fan 1 0.80"));
    CHECK_EQ(t.One("ssr 1 off"), std::string("ok ssr 1 off"));
    t.hal.FireTrip(kFan0 + 1);
    t.hal.FireTrip(kSsr0 + 1);
    CHECK(StartsWith(t.One("fan 1 0.5"), "err fan 1 failsafe tripped"));
    t.ctl->MaintenanceTick();
    CHECK(!t.ctl->Snapshot(kFan0 + 1).tripped);
    CHECK_EQ(t.hal.Get(kFan0 + 1).duty, 0.8);   // re-driven to the commanded duty
    CHECK(t.hal.Get(kFan0 + 1).failsafe);
    CHECK(!t.hal.Get(kSsr0 + 1).state);
    CHECK(t.hal.Get(kSsr0 + 1).failsafe);
    CHECK_EQ(t.One("fan 1 0.5"), std::string("ok fan 1 0.50"));
}

static void TestStaggerSkipsLostRelays() {
    Rig t;
    t.ctl->CreateAll();
    t.ctl->OpenRelaysBlocking();
    t.onSlot = [&](int slot) {
        if (slot == 0) {
            t.hal.FireDetach(3);                 // lost before its slot
            CHECK_EQ(t.One("close 5"), std::string("ok close 5"));
            t.hal.FireTrip(5);                   // closed early, then tripped
        }
    };
    bool fatal = false;
    try { t.ctl->RunStagger(); } catch (const Fatal&) { fatal = true; }
    CHECK(!fatal);
    for (auto& c : t.hal.Calls()) { CHECK(c != "setstate 3 1"); }
    CHECK(!t.hal.Get(3).state);
    CHECK(!t.hal.Get(5).state);
    CHECK(t.hal.Get(7).state);
}

// A slow HAL call inside a command (or an attach) must not stall the heartbeat.
static void TestSlowCommandDoesNotBlockHeartbeat() {
    Rig t;
    t.Start();
    t.hal.holdDuty = true;
    std::thread cmd([&] { t.ctl->Handle("fan 0 0.3"); });
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    std::atomic<bool> done{false};
    int before = t.hal.Get(0).resets;
    std::thread hb([&] { t.ctl->HeartbeatTick(); done = true; });
    for (int i = 0; i < 100 && !done; ++i) std::this_thread::sleep_for(std::chrono::milliseconds(10));
    CHECK(done.load());
    // `state` also answers meanwhile.
    CHECK_EQ(t.ctl->Handle("state").size(), 17ul);
    t.hal.holdDuty = false;
    cmd.join();
    hb.join();
    CHECK_EQ(t.hal.Get(0).resets, before + 1);
    CHECK_EQ(t.hal.Get(kFan0).duty, 0.3);
}

static void TestStopDuringBlockingStartup() {
    Rig t;
    t.ctl->CreateAll();
    t.running = false;
    t.ctl->OpenRelaysBlocking();   // returns without opening anything
    for (auto& c : t.hal.Calls()) CHECK(!StartsWith(c, "openwait"));
}

static void TestFansAndSsr() {
    Rig t;
    t.ctl->CreateAll();
    t.ctl->OpenRelaysBlocking();
    t.ctl->OpenDynamic();
    // Dynamic: nothing driven before attach.
    CHECK(t.hal.Get(kFan0).opened);
    CHECK_EQ(t.hal.Get(kFan0).duty, -1.0);
    CHECK(StartsWith(t.One("fan 0 0.3"), "err fan 0 not attached"));
    CHECK(StartsWith(t.One("ssr 0 off"), "err ssr 0 not attached"));
    for (int i = kFan0; i < t.ctl->Size(); ++i) t.hal.FireAttach(i);
    for (int i = 0; i < 4; ++i) {
        CHECK_EQ(t.hal.Get(kFan0 + i).duty, 0.5);
        CHECK(t.hal.Get(kFan0 + i).failsafe);
        CHECK(t.hal.Get(kSsr0 + i).state);
        CHECK(t.hal.Get(kSsr0 + i).failsafe);
    }
    CHECK_EQ(t.One("fan 1 0.25"), std::string("ok fan 1 0.25"));
    CHECK_EQ(t.hal.Get(kFan0 + 1).duty, 0.25);
    CHECK_EQ(t.One("fan 1 1"), std::string("ok fan 1 1.00"));
    CHECK_EQ(t.One("fan 1 0"), std::string("ok fan 1 0.00"));
    CHECK_EQ(t.One("ssr 2 off"), std::string("ok ssr 2 off"));
    CHECK(!t.hal.Get(kSsr0 + 2).state);
    CHECK(t.ctl->Snapshot(kSsr0 + 2).armed);
    CHECK_EQ(t.One("ssr 2 on"), std::string("ok ssr 2 on"));
    CHECK(t.hal.Get(kSsr0 + 2).state);
    CHECK_EQ(t.hal.Get(kSsr0 + 2).enables, 1);

    // Re-attach re-drives the COMMANDED value (not the default).
    CHECK_EQ(t.One("fan 3 0.75"), std::string("ok fan 3 0.75"));
    CHECK_EQ(t.One("ssr 3 off"), std::string("ok ssr 3 off"));
    t.hal.FireDetach(kFan0 + 3); t.hal.FireAttach(kFan0 + 3);
    t.hal.FireDetach(kSsr0 + 3); t.hal.FireAttach(kSsr0 + 3);
    CHECK_EQ(t.hal.Get(kFan0 + 3).duty, 0.75);
    CHECK(!t.hal.Get(kSsr0 + 3).state);
    CHECK(t.hal.Get(kSsr0 + 3).failsafe);

    t.hal.failDuty.insert(kFan0);
    CHECK(StartsWith(t.One("fan 0 0.9"), "err fan 0 setDutyCycle failed"));
    CHECK_EQ(t.ctl->Snapshot(kFan0).duty, 0.5);
    for (const char* b : {"fan 4 0.5", "fan 0 2", "fan 0", "ssr 4 on", "ssr 0 maybe"})
        CHECK(StartsWith(t.One(b), "err "));
}

static void TestUnconfiguredFanChannel() {
    Config c;
    c.fan.channels = {0, 1};
    c.ssr.channels = {2};
    Rig t(c);
    t.Start();
    CHECK_EQ(t.ctl->Size(), 8 + 2 + 1);
    CHECK_EQ(t.One("fan 1 0.4"), std::string("ok fan 1 0.40"));
    CHECK_EQ(t.One("fan 2 0.4"), std::string("err fan 2 not configured"));
    CHECK_EQ(t.One("ssr 0 on"), std::string("err ssr 0 not configured"));
    CHECK_EQ(t.One("ssr 2 off"), std::string("ok ssr 2 off"));
}

static void TestStateFormat() {
    Rig t;
    t.Start();
    t.One("open 1");
    t.One("fan 2 0.333");
    t.One("ssr 3 off");
    t.hal.FireDetach(7);
    std::vector<std::string> want = {
        "relay 0 cmd=closed attached=1", "relay 1 cmd=open attached=1",
        "relay 2 cmd=closed attached=1", "relay 3 cmd=closed attached=1",
        "relay 4 cmd=closed attached=1", "relay 5 cmd=closed attached=1",
        "relay 6 cmd=closed attached=1", "relay 7 cmd=open attached=0",
        "fan 0 duty=0.50 attached=1", "fan 1 duty=0.50 attached=1",
        "fan 2 duty=0.33 attached=1", "fan 3 duty=0.50 attached=1",
        "ssr 0 cmd=on attached=1", "ssr 1 cmd=on attached=1",
        "ssr 2 cmd=on attached=1", "ssr 3 cmd=off attached=1",
        "end",
    };
    CHECK_EQ(t.ctl->Handle("state"), want);
}

// Reply strings exactly as relay-mqtt's parser accepts them
// (protocol.rs: Command::is_ok_reply, parse_relay_line; bridge.rs: err replies).
static void TestRepliesMatchRelayMqtt() {
    Rig t;
    t.Start();
    for (int ch = 0; ch < 8; ++ch) {
        const std::string c = std::to_string(ch);
        CHECK_EQ(t.One("open " + c), "ok " + std::string("open ") + c);   // format!("ok {} {}", verb, ch)
        CHECK_EQ(t.One("close " + c), "ok " + std::string("close ") + c);
    }
    t.hal.failSetState.insert(0);
    std::string e = t.One("open 0");
    CHECK(e == "err" || StartsWith(e, "err "));
    // state: every relay line is exactly "relay <d> cmd=<open|closed> attached=<0|1>"
    // (single spaces, one digit), all 8 present once, terminated by "end".
    auto st = t.ctl->Handle("state");
    CHECK_EQ(st.back(), std::string("end"));
    int relays = 0;
    for (auto& l : st) {
        if (!StartsWith(l, "relay ")) continue;
        char d = l[6];
        std::string rest = l.substr(7);
        CHECK(d >= '0' && d <= '7');
        CHECK_EQ(d - '0', relays);
        CHECK(rest == " cmd=open attached=1" || rest == " cmd=closed attached=1" ||
              rest == " cmd=open attached=0" || rest == " cmd=closed attached=0");
        ++relays;
    }
    CHECK_EQ(relays, 8);
}

static void TestHeartbeatFeedsOnlyArmed() {
    Rig t;
    t.ctl->CreateAll();
    t.ctl->OpenRelaysBlocking();
    t.ctl->OpenDynamic();
    t.ctl->HeartbeatTick();
    for (int i = 0; i < t.ctl->Size(); ++i) CHECK_EQ(t.hal.Get(i).resets, 0);
    t.hal.FireAttach(kFan0);
    t.ctl->HeartbeatTick();
    CHECK_EQ(t.hal.Get(kFan0).resets, 1);
    CHECK_EQ(t.hal.Get(kFan0 + 1).resets, 0);
    CHECK_EQ(t.hal.Get(0).resets, 0);
    t.ctl->RunStagger();
    t.ctl->HeartbeatTick();
    for (int i = 0; i < 8; ++i) CHECK(t.hal.Get(i).resets >= 1);
}

static void TestFatalStartup() {
    {
        Rig t;
        t.hal.failOpenWait.insert(2);
        t.ctl->CreateAll();
        bool fatal = false;
        try { t.ctl->OpenRelaysBlocking(); } catch (const Fatal&) { fatal = true; }
        CHECK(fatal);
    }
    {
        Rig t;
        t.ctl->CreateAll();
        t.ctl->OpenRelaysBlocking();
        t.hal.failSetState.insert(0);
        bool fatal = false;
        try { t.ctl->RunStagger(); } catch (const Fatal&) { fatal = true; }
        CHECK(fatal);   // relay_controller: Check(setState(on)) is fatal in the stagger
    }
}

static void TestShutdown() {
    Rig t;
    t.Start();
    t.ctl->Shutdown();
    for (int i = 0; i < t.ctl->Size(); ++i) {
        auto o = t.hal.Get(i);
        CHECK(o.closed);
        if (i >= kFan0 && i < kSsr0) CHECK_EQ(o.duty, 0.0);
        else CHECK(!o.state);
    }
}

// ---- CLI ----------------------------------------------------------------------------
static bool Args(std::vector<const char*> v, Config& c, int& code) {
    v.insert(v.begin(), "rack_controller");
    return ParseArgs(static_cast<int>(v.size()), const_cast<char**>(v.data()), c, code);
}

static void TestArgs() {
    Config c; int code = -1;
    CHECK(Args({}, c, code));
    CHECK_EQ(c.serial, 592109); CHECK_EQ(c.failsafeMs, 1000u); CHECK_EQ(c.duty, 0.5);
    CHECK_EQ(c.socketPath, std::string("/run/rack_controller/ctl.sock"));
    CHECK_EQ(c.fan.hubPort, 2); CHECK_EQ(c.ssr.hubPort, 5);

    Config d;
    CHECK(Args({"--serial", "-1", "--fan", "1:0,2", "--ssr", "0", "--duty", "0.7",
                "--failsafe-ms", "1500", "--socket", "/tmp/x.sock"}, d, code));
    CHECK_EQ(d.serial, -1); CHECK_EQ(d.fan.hubPort, 1);
    CHECK_EQ(d.fan.channels, (std::vector<int>{0, 2}));
    CHECK_EQ(d.ssr.hubPort, 0); CHECK_EQ(d.ssr.channels.size(), 4ul);
    CHECK_EQ(d.duty, 0.7); CHECK_EQ(d.failsafeMs, 1500u);
    CHECK_EQ(d.socketPath, std::string("/tmp/x.sock"));

    const std::vector<std::vector<const char*>> bad = {
        {"--serial"}, {"--serial", "abc"}, {"--serial", "0"}, {"--serial", "-2"},
        {"--fan", "3"}, {"--ssr", "4:0"}, {"--fan", "5"}, {"--fan", "2:0,0"}, {"--fan", "6"},
        {"--fan", "2:4"}, {"--duty", "1.5"}, {"--duty", "nan"}, {"--failsafe-ms", "10"},
        {"--fan", " 2"}, {"--fan", "+2"}, {"--fan", "2:0,"}, {"--fan", "2:"}, {"--fan", "22"},
        {"--fan", "2:0,,1"}, {"--fan", "2:01"}, {"--ssr", "-5"},
        {"--socket", "relative.sock"}, {"--socket", ""}, {"--bogus"}, {"--static"},
    };
    for (auto& b : bad) {
        Config e; code = 0;
        CHECK(!Args(b, e, code));
        CHECK_EQ(code, 2);
    }
    std::string longPath = "/" + std::string(200, 'a');
    Config e;
    CHECK(!Args({"--socket", longPath.c_str()}, e, code));
}

// ---- Socket server ------------------------------------------------------------------
struct TmpDir {
    std::string path;
    TmpDir() {
        char tmpl[] = "/tmp/rct-XXXXXX";   // short: sun_path is ~104 bytes on macOS
        const char* p = mkdtemp(tmpl);
        path = p ? p : "";
    }
    ~TmpDir() {
        std::string sock = path + "/ctl.sock";
        unlink(sock.c_str());
        rmdir(path.c_str());
    }
};

static int Connect(const std::string& path) {
    for (int tries = 0; tries < 200; ++tries) {
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        sockaddr_un a{};
        a.sun_family = AF_UNIX;
        std::strncpy(a.sun_path, path.c_str(), sizeof a.sun_path - 1);
        if (connect(fd, reinterpret_cast<sockaddr*>(&a), sizeof a) == 0) {
            timeval tv{5, 0};
            setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
#ifdef SO_NOSIGPIPE
            int one = 1;
            setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
#endif
            return fd;
        }
        close(fd);
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    return -1;
}

static void SendAll(int fd, const std::string& s) {
    size_t off = 0;
    while (off < s.size()) {
        ssize_t n = send(fd, s.data() + off, s.size() - off, 0);
        if (n <= 0) return;
        off += static_cast<size_t>(n);
    }
}

struct LineReader {
    int fd;
    std::string buf;
    // Returns false on EOF/timeout.
    bool Line(std::string& out) {
        for (;;) {
            size_t nl = buf.find('\n');
            if (nl != std::string::npos) {
                out = buf.substr(0, nl);
                buf.erase(0, nl + 1);
                return true;
            }
            char tmp[256];
            ssize_t n = recv(fd, tmp, sizeof tmp, 0);
            if (n <= 0) return false;
            buf.append(tmp, static_cast<size_t>(n));
        }
    }
    std::string Next() { std::string l; return Line(l) ? l : "<eof>"; }
};

static void TestSocketServer() {
    TmpDir dir;
    CHECK(!dir.path.empty());
    const std::string sock = dir.path + "/ctl.sock";
    Rig t;
    t.Start();
    std::atomic<bool> running{true};
    SocketServer srv(*t.ctl, sock, running);
    std::thread th([&] { srv.Run(); });

    int fd = Connect(sock);
    CHECK(fd >= 0);
    struct stat st{};
    CHECK(lstat(sock.c_str(), &st) == 0);
    CHECK(S_ISSOCK(st.st_mode));
    CHECK_EQ(static_cast<int>(st.st_mode & 0777), 0660);

    LineReader r{fd, ""};
    // Many requests on one connection, one at a time.
    SendAll(fd, "open 3\n");
    CHECK_EQ(r.Next(), std::string("ok open 3"));
    SendAll(fd, "close 3\n");
    CHECK_EQ(r.Next(), std::string("ok close 3"));
    // Pipelined in one write, including a split line.
    SendAll(fd, "open 4\nfan 0 0.5\nssr 1 off\nbogus\nop");
    SendAll(fd, "en 9\n");
    CHECK_EQ(r.Next(), std::string("ok open 4"));
    CHECK_EQ(r.Next(), std::string("ok fan 0 0.50"));
    CHECK_EQ(r.Next(), std::string("ok ssr 1 off"));
    CHECK(StartsWith(r.Next(), "err "));
    CHECK(StartsWith(r.Next(), "err "));
    // Oversize: rejected, connection keeps working. 128 bytes is still accepted
    // as a (malformed) request; 129 is too long.
    SendAll(fd, std::string(129, 'x') + "\n");
    CHECK_EQ(r.Next(), std::string("err line too long"));
    SendAll(fd, std::string(128, 'x') + "\n");
    CHECK_EQ(r.Next(), std::string("err unknown command"));
    SendAll(fd, std::string(5000, 'y') + "\nstate\n");
    CHECK_EQ(r.Next(), std::string("err line too long"));
    std::vector<std::string> st1;
    for (std::string l; (l = r.Next()) != "end" && l != "<eof>";) st1.push_back(l);
    CHECK_EQ(st1.size(), 16ul);
    CHECK_EQ(st1[4], std::string("relay 4 cmd=open attached=1"));
    CHECK_EQ(st1[13], std::string("ssr 1 cmd=off attached=1"));
    // Empty line.
    SendAll(fd, "\n");
    CHECK_EQ(r.Next(), std::string("err empty request"));

    // A second, concurrent client.
    int fd2 = Connect(sock);
    CHECK(fd2 >= 0);
    LineReader r2{fd2, ""};
    SendAll(fd2, "open 6\n");
    CHECK_EQ(r2.Next(), std::string("ok open 6"));
    SendAll(fd, "state\n");
    std::vector<std::string> st2;
    for (std::string l; (l = r.Next()) != "end" && l != "<eof>";) st2.push_back(l);
    CHECK_EQ(st2[6], std::string("relay 6 cmd=open attached=1"));

    // A client that disconnects mid-request doesn't hurt the server.
    int fd3 = Connect(sock);
    SendAll(fd3, "open 1");
    close(fd3);
    // Half-close: the reply still arrives.
    int fd4 = Connect(sock);
    SendAll(fd4, "close 6\n");
    shutdown(fd4, SHUT_WR);
    LineReader r4{fd4, ""};
    CHECK_EQ(r4.Next(), std::string("ok close 6"));
    CHECK_EQ(r4.Next(), std::string("<eof>"));
    close(fd4);
    // Abrupt close with unread replies queued (would SIGPIPE without care).
    int fd5 = Connect(sock);
    std::string spam;
    for (int i = 0; i < 200; ++i) spam += "state\n";
    SendAll(fd5, spam);
    close(fd5);

    SendAll(fd2, "close 4\n");
    CHECK_EQ(r2.Next(), std::string("ok close 4"));
    CHECK(t.ctl->Snapshot(1).cmdOn);   // the partial "open 1" was never executed

    close(fd);
    close(fd2);
    running = false;
    th.join();
    CHECK(lstat(sock.c_str(), &st) != 0);   // unlinked on shutdown
}

static void TestSocketStaleAndNonSocket() {
    TmpDir dir;
    const std::string sock = dir.path + "/ctl.sock";
    Rig t;
    t.Start();
    // A plain file at the path is never deleted; the server keeps retrying.
    { FILE* f = std::fopen(sock.c_str(), "w"); if (f) std::fclose(f); }
    std::atomic<bool> running{true};
    SocketServer srv(*t.ctl, sock, running);
    std::thread th([&] { srv.Run(); });
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    struct stat st{};
    CHECK(lstat(sock.c_str(), &st) == 0);
    CHECK(S_ISREG(st.st_mode));
    unlink(sock.c_str());
    // Next retry (2 s) binds.
    int fd = Connect(sock);
    for (int i = 0; fd < 0 && i < 10; ++i) fd = Connect(sock);
    CHECK(fd >= 0);
    LineReader r{fd, ""};
    SendAll(fd, "close 0\n");
    CHECK_EQ(r.Next(), std::string("ok close 0"));
    close(fd);
    running = false;
    th.join();
}

int main() {
    TestParserValid();
    TestParserInvalid();
    TestStartupAndStagger();
    TestStaggerStopsOnSignal();
    TestOpenDuringStaggerSkipsRelay();
    TestStartOpenKeepsAllRelaysOff();
    TestStartOpenFlagParses();
    TestCloseDuringStaggerArmsEarly();
    TestRelayCommands();
    TestSetStateFailureIsErrNotExit();
    TestDetachAndReattach();
    TestFailsafeTrip();
    TestFanTripRecovers();
    TestStaggerSkipsLostRelays();
    TestSlowCommandDoesNotBlockHeartbeat();
    TestStopDuringBlockingStartup();
    TestFansAndSsr();
    TestUnconfiguredFanChannel();
    TestStateFormat();
    TestRepliesMatchRelayMqtt();
    TestHeartbeatFeedsOnlyArmed();
    TestFatalStartup();
    TestShutdown();
    TestArgs();
    TestSocketServer();
    TestSocketStaleAndNonSocket();
    std::printf("%d checks, %d failures\n", g_checks, g_fail);
    return g_fail == 0 ? 0 : 1;
}
