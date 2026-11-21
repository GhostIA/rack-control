// test_server: the real rack_controller core + socket server on a fake HAL
// (every channel attached, stagger without delays). Used only by the
// relay-mqtt cross-check in tests/client_xcheck; never deployed.
//
// usage: test_server <socket-path> [--fail-relay CH]...

#include "../core.h"
#include "fake_hal.h"

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstring>
#include <thread>

namespace {
std::atomic<bool> g_running{true};
void OnSignal(int) { g_running.store(false); }
}

int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: %s <socket-path> [--fail-relay CH]...\n", argv[0]);
        return 2;
    }
    std::signal(SIGINT, OnSignal);
    std::signal(SIGTERM, OnSignal);
    std::signal(SIGPIPE, SIG_IGN);

    rc::FakeHal hal;
    hal.attachOnOpen = true;
    rc::Config cfg;
    cfg.socketPath = argv[1];
    rc::Controller ctl(hal, cfg, g_running);
    ctl.SetSleep([](std::chrono::milliseconds) {});
    ctl.CreateAll();
    std::thread hb([&ctl] { ctl.HeartbeatLoop(); });
    ctl.OpenDynamic();
    ctl.OpenRelaysBlocking();
    ctl.RunStagger();   // before the socket: relays are all closed when clients connect
    // Failure injection after startup (a startup failure is fatal by design).
    for (int i = 2; i + 1 < argc; i += 2) {
        if (!std::strcmp(argv[i], "--fail-relay")) hal.failSetState.insert(std::atoi(argv[i + 1]));
    }
    rc::SocketServer srv(ctl, cfg.socketPath, g_running);
    std::thread st([&srv] { srv.Run(); });
    while (g_running.load()) {
        ctl.MaintenanceTick();
        std::this_thread::sleep_for(std::chrono::milliseconds(20));
    }
    st.join();
    hb.join();
    ctl.Shutdown();
    return 0;
}
