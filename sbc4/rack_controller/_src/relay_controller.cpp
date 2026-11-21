// relay_controller.cpp
//
// Staggered power-on for 2x Phidget REL1000 (4ch each) on an SBC4's built-in
// VINT hub, ports 3 and 4. All 8 channels are brought up one at a time, then
// held on by continuously resetting a per-channel hardware failsafe.
//
// Relays are wired NORMALLY OPEN: output TRUE closes the relay (load on),
// output FALSE opens it (load off). The failsafe drives outputs FALSE on loss
// of communication, so if this program crashes, is killed, or the SBC4 stops
// servicing it, every relay opens on its own and all loads cut. (NO wiring is
// required for this fail-to-off behavior; the failsafe only ever drives FALSE.)
//
// Runs locally on the SBC4 (local VINT access). Network loss does NOT cut power.
//
// Build on the SBC4:
//   g++ -std=c++17 relay_controller.cpp -lphidget22 -lpthread -o relay_controller

#include <phidget22.h>

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <thread>
#include <vector>

namespace {

// ---- Topology ----------------------------------------------------------------
// Two REL1000s on the SBC4's built-in VINT hub, ports 3 and 4, 4 channels each.
// All 8 channels are used. Order below is the staggered power-on order.
struct ChannelMap { int hubPort; int channel; };

const std::vector<ChannelMap> kChannels = {
    {3, 0}, {3, 1}, {3, 2}, {3, 3},   // REL1000 on hub port 3
    {4, 0}, {4, 1}, {4, 2}, {4, 3},   // REL1000 on hub port 4
};

// SBC4 serial number. Set this to the real serial ("be specific" — Phidgets'
// own best practice) so nothing else on the bus can ever be matched by accident.
// -1 matches any device; fine on an isolated SBC4 but less robust.
constexpr int kSBC4Serial = -1;   // TODO: set to the SBC4's actual serial number

// ---- Timing ------------------------------------------------------------------
constexpr uint32_t kFailsafeMs = 1000;     // outputs open ~1s after heartbeat stops
const auto kResetInterval = std::chrono::milliseconds(kFailsafeMs / 3); // ⅓-time rule
const auto kStaggerDelay  = std::chrono::milliseconds(2500);            // between power-ons
constexpr uint32_t kAttachTimeoutMs = 5000;

// If resetFailsafe keeps failing for a channel, that channel's heartbeat isn't
// getting through and the hardware will trip it. Log loudly after this many
// consecutive failures (per channel) so a real VINT problem surfaces instead
// of silently letting a machine drop.
constexpr int kResetFailWarnThreshold = 5;

std::atomic<bool> g_running{true};

void OnSignal(int) { g_running.store(false); }  // SIGINT/SIGTERM -> graceful shutdown

const char* ErrStr(PhidgetReturnCode rc) {
    const char* desc = nullptr;
    Phidget_getErrorDescription(rc, &desc);
    return desc ? desc : "?";
}

void Check(PhidgetReturnCode rc, const char* what) {
    if (rc != EPHIDGET_OK) {
        std::fprintf(stderr, "FATAL: %s failed: %s (%d)\n", what, ErrStr(rc), rc);
        std::exit(1);
    }
}

// ---- Event handlers (Phidgets' recommended event-driven structure) -----------
// Context passed to handlers so their logs identify which channel fired.
struct ChannelCtx { int index; int hubPort; int channel; };

void CCONV OnAttach(PhidgetHandle /*h*/, void* ctx) {
    auto* c = static_cast<ChannelCtx*>(ctx);
    std::fprintf(stderr, "ATTACH: channel %d (port %d ch %d)\n",
                 c->index, c->hubPort, c->channel);
    // open() is set to retry forever, so a re-attach after a VINT blip lands
    // here. The main loop's resetFailsafe resumes automatically on this handle.
}

void CCONV OnDetach(PhidgetHandle /*h*/, void* ctx) {
    auto* c = static_cast<ChannelCtx*>(ctx);
    std::fprintf(stderr, "DETACH: channel %d (port %d ch %d) -- lost from VINT\n",
                 c->index, c->hubPort, c->channel);
    // No action needed to make the load safe: a detached REL1000 is unpowered
    // on VINT, so its NO relay is already open (load off). We just log and let
    // open()'s auto-reconnect bring it back, where OnAttach re-arms via the loop.
}

void CCONV OnError(PhidgetHandle /*h*/, void* ctx, Phidget_ErrorEventCode code,
                   const char* description) {
    auto* c = static_cast<ChannelCtx*>(ctx);
    std::fprintf(stderr, "ERROR: channel %d (port %d ch %d): %s (%d)\n",
                 c->index, c->hubPort, c->channel,
                 description ? description : "?", code);
    // A failsafe trip surfaces here too; the channel then rejects input until
    // reopened, which a service restart handles cleanly.
}

// Reset the failsafe on the first `count` channels (those currently armed).
void FeedFailsafe(const std::vector<PhidgetDigitalOutputHandle>& ch,
                  int count, std::vector<int>& failStreak) {
    for (int i = 0; i < count; ++i) {
        PhidgetReturnCode rc = PhidgetDigitalOutput_resetFailsafe(ch[i]);
        if (rc == EPHIDGET_OK) {
            failStreak[i] = 0;
        } else if (++failStreak[i] == kResetFailWarnThreshold) {
            std::fprintf(stderr, "WARN: channel %d failsafe reset failing: %s (%d)\n",
                         i, ErrStr(rc), rc);
        }
    }
}

} // namespace

int main() {
    std::signal(SIGINT,  OnSignal);
    std::signal(SIGTERM, OnSignal);

    const int n = static_cast<int>(kChannels.size());
    std::vector<PhidgetDigitalOutputHandle> ch(n);
    std::vector<ChannelCtx> ctx(n);
    std::vector<int> failStreak(n, 0);

    // ---- Open every channel, wire up events, start FALSE, then arm failsafe ----
    for (int i = 0; i < n; ++i) {
        ctx[i] = ChannelCtx{i, kChannels[i].hubPort, kChannels[i].channel};

        Check(PhidgetDigitalOutput_create(&ch[i]), "create");
        auto h = reinterpret_cast<PhidgetHandle>(ch[i]);

        // Be specific: serial + hub port + channel.
        Check(Phidget_setDeviceSerialNumber(h, kSBC4Serial), "setDeviceSerialNumber");
        Check(Phidget_setHubPort(h, kChannels[i].hubPort),   "setHubPort");
        Check(Phidget_setChannel(h, kChannels[i].channel),   "setChannel");

        // Event handlers (registered before open so we catch the first attach).
        Check(Phidget_setOnAttachHandler(h, OnAttach, &ctx[i]), "setOnAttachHandler");
        Check(Phidget_setOnDetachHandler(h, OnDetach, &ctx[i]), "setOnDetachHandler");
        Check(Phidget_setOnErrorHandler(h,  OnError,  &ctx[i]), "setOnErrorHandler");

        Check(Phidget_openWaitForAttachment(h, kAttachTimeoutMs), "openWaitForAttachment");

        Check(PhidgetDigitalOutput_setState(ch[i], 0), "setState(off)");  // start open
        // NOTE: do NOT arm the failsafe here. A channel's failsafe timer starts
        // the instant it's enabled, so arming all 8 up front means channels that
        // haven't reached their turn in the stagger would time out (stagger delay
        // > failsafe time) before we ever feed them. We arm each channel only as
        // it's switched on, below.
    }
    std::fprintf(stderr, "All %d channels open (failsafe %ums, armed as each turns on)\n",
                 n, kFailsafeMs);

    // ---- Staggered power-on -------------------------------------------------
    // For each channel: close the relay, then arm its failsafe. From that point
    // it must be fed. During the stagger delay we feed every channel armed so
    // far, so none time out while later ones are still coming up.
    int armed = 0;  // channels [0, armed-1] are on and have their failsafe enabled
    for (int i = 0; i < n && g_running.load(); ++i) {
        Check(PhidgetDigitalOutput_setState(ch[i], 1), "setState(on)");       // close -> load on
        Check(PhidgetDigitalOutput_enableFailsafe(ch[i], kFailsafeMs), "enableFailsafe");
        armed = i + 1;
        std::fprintf(stderr, "Channel %d (port %d ch %d) ON\n",
                     i, kChannels[i].hubPort, kChannels[i].channel);

        const auto deadline = std::chrono::steady_clock::now() + kStaggerDelay;
        while (std::chrono::steady_clock::now() < deadline && g_running.load()) {
            FeedFailsafe(ch, armed, failStreak);
            std::this_thread::sleep_for(kResetInterval);
        }
    }

    // ---- Main heartbeat loop ------------------------------------------------
    // Hold everything on by resetting the failsafe faster than it expires.
    // Stop resetting (crash/kill/power loss) and the hardware opens all relays.
    while (g_running.load()) {
        FeedFailsafe(ch, n, failStreak);   // all n channels now armed and on
        std::this_thread::sleep_for(kResetInterval);
    }

    // ---- Graceful shutdown (SIGINT/SIGTERM) ---------------------------------
    // Explicitly open all relays. On a crash we'd skip this, but the failsafe
    // would open them ~kFailsafeMs after the last reset anyway.
    std::fprintf(stderr, "Shutting down: opening all relays\n");
    for (int i = 0; i < n; ++i) {
        PhidgetDigitalOutput_setState(ch[i], 0);
        auto h = reinterpret_cast<PhidgetHandle>(ch[i]);
        Phidget_close(h);
        PhidgetDigitalOutput_delete(&ch[i]);
    }
    return 0;
}

