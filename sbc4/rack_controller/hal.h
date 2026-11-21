// hal.h -- thin hardware abstraction between rack_controller's logic and the
// Phidget22 library. The real implementation (hal_phidget.h) is a one-to-one
// wrapper around the Phidget calls the two original programs made; the tests
// use a fake (tests/fake_hal.h). Nothing here includes phidget22.h.
//
// Channels are identified by their index in the controller's channel table.
#pragma once

#include <cstdint>

namespace rc {

// A Phidget return code. 0 is EPHIDGET_OK.
using Rc = int;
constexpr Rc kOk = 0;

// Event sink for attach / detach / error events. The real HAL calls these
// from the Phidget event thread.
class HalListener {
public:
    virtual ~HalListener() = default;
    virtual void OnAttach(int idx) = 0;
    virtual void OnDetach(int idx) = 0;
    // failsafeTrip is true for the Phidget "failsafe tripped" error event
    // (EEPHIDGET_FAILSAFE): the channel then rejects input until reopened.
    virtual void OnError(int idx, int code, const char* description, bool failsafeTrip) = 0;
};

class Hal {
public:
    virtual ~Hal() = default;

    // Create the DigitalOutput handle for `idx`, address it (serial, hub port,
    // channel) and register the attach/detach/error handlers. On failure,
    // *what names the step that failed (e.g. "setHubPort").
    virtual Rc Create(int idx, int serial, int hubPort, int channel,
                      HalListener* listener, const char** what) = 0;
    virtual Rc OpenWaitForAttachment(int idx, uint32_t timeoutMs) = 0;  // blocking
    virtual Rc Open(int idx) = 0;                                       // non-blocking
    virtual Rc SetState(int idx, bool on) = 0;
    virtual Rc SetDutyCycle(int idx, double duty) = 0;
    virtual Rc EnableFailsafe(int idx, uint32_t ms) = 0;
    virtual Rc ResetFailsafe(int idx) = 0;
    // Phidget_close then Phidget_open (non-blocking): clears a failsafe trip;
    // the attach handler fires again when the channel is back.
    virtual Rc Reopen(int idx) = 0;
    // Phidget_close + delete. Safe on a handle that never opened.
    virtual void Close(int idx) = 0;

    virtual const char* ErrStr(Rc rc) = 0;
    // True if rc means the channel's failsafe has tripped (EPHIDGET_FAILSAFE).
    virtual bool IsFailsafeRc(Rc rc) = 0;
};

} // namespace rc
