// rack_controller.cpp
//
// The single owner of the SBC4's built-in VINT hub (HUB0004, serial 592109).
// Merges relay_controller.cpp and fan_control.cpp (_src/, kept unchanged as the
// rollback baseline) into one process, because the hub does not allow two
// processes to open its channels:
//
//   hub ports 3, 4  2x REL1000  relays, relay ch 0-7 (0-3 = port 3, 4-7 = port 4)
//   hub port 2      OUT1100     fans, PWM duty (default 0.5)
//   hub port 5      REL1100     SSR on/off (default on)
//
// Relays keep relay_controller's BLOCKING startup verbatim: openWaitForAttachment
// (fatal on failure), start FALSE, then a one-time 2.5 s stagger that arms each
// relay's failsafe only as it turns on. Fans and SSR keep fan_control's dynamic
// model: non-blocking open, driven and armed on attach, re-driven on re-attach.
// One heartbeat resets every armed channel's failsafe each 1/3 of its window.
//
// Everything is wired NORMALLY OPEN / fail-to-off: if this process dies or
// hangs, the hardware failsafe drives every armed output FALSE (relays open =
// PCs off, fans 0, SSR off). Network loss does NOT cut power.
//
// Control: a local Unix socket (default /run/rack_controller/ctl.sock, 0660,
// group rackctl via systemd), one request per line:
//   open|close <ch 0-7>, fan <ch 0-3> <duty 0.0-1.0>, ssr <ch 0-3> on|off, state
// The relay-mqtt sidecar uses it; this program has no network code. Commanded
// state is not persisted: a restart brings all 8 relays back on via the stagger.
//
// Build on the SBC4:  make   (g++ -std=c++17 ... -lphidget22 -lpthread)

#include "core.h"
#include "hal_phidget.h"

#include <atomic>
#include <chrono>
#include <csignal>
#include <thread>

namespace {

std::atomic<bool> g_running{true};

void OnSignal(int) { g_running.store(false); }  // SIGINT/SIGTERM -> graceful shutdown

} // namespace

int main(int argc, char** argv) {
    rc::Config cfg;
    int exitCode = 0;
    if (!rc::ParseArgs(argc, argv, cfg, exitCode)) return exitCode;

    std::signal(SIGINT,  OnSignal);
    std::signal(SIGTERM, OnSignal);
    std::signal(SIGPIPE, SIG_IGN);   // a vanished socket client must never kill the hub owner

    rc::PhidgetHal hal;
    rc::Controller ctl(hal, cfg, g_running);

    // 1. One create loop for every channel (fatal on error, as relay_controller).
    ctl.CreateAll();
    // 2. One heartbeat for every armed channel, from before anything is armed.
    std::thread heartbeat([&ctl] { ctl.HeartbeatLoop(); });
    // 3. Fans and SSR first (cooling as early as possible): non-blocking open;
    //    each is driven and armed on attach.
    ctl.OpenDynamic();
    // 4. Relays: blocking open, all start FALSE (fatal on error).
    ctl.OpenRelaysBlocking();
    // 5. Control socket, up before the stagger so `open <ch>` can skip a relay.
    rc::SocketServer server(ctl, cfg.socketPath, g_running);
    std::thread socket([&server] { server.Run(); });
    // 6. One-time staggered relay power-on.
    ctl.RunStagger();

    // Main thread from here on: reopen failsafe-tripped channels.
    while (g_running.load()) {
        ctl.MaintenanceTick();
        std::this_thread::sleep_for(std::chrono::milliseconds(100));
    }

    // Graceful shutdown (SIGINT/SIGTERM). On a crash we'd skip this, but the
    // failsafe would drive every armed output off ~failsafe-ms later anyway.
    socket.join();      // closes clients, unlinks the socket
    heartbeat.join();
    ctl.Shutdown();     // relays open, fans 0, SSR off, handles closed
    return 0;
}
