// hal_phidget.h -- the real rc::Hal: a one-to-one wrapper around the Phidget22
// calls relay_controller.cpp and fan_control.cpp make. Only rack_controller.cpp
// includes this (the tests use tests/fake_hal.h instead).
#pragma once

#include "hal.h"

#include <phidget22.h>

#include <memory>
#include <vector>

namespace rc {

class PhidgetHal : public Hal {
public:
    ~PhidgetHal() override = default;

    Rc Create(int idx, int serial, int hubPort, int channel,
              HalListener* listener, const char** what) override {
        if (idx < 0) { *what = "create"; return EPHIDGET_INVALIDARG; }
        const size_t i = static_cast<size_t>(idx);
        if (h_.size() <= i) { h_.resize(i + 1, nullptr); ctx_.resize(i + 1); }
        ctx_[i].reset(new Ctx{listener, idx});

        PhidgetReturnCode rc;
        *what = "create";
        if ((rc = PhidgetDigitalOutput_create(&h_[i])) != EPHIDGET_OK) return rc;
        auto h = reinterpret_cast<PhidgetHandle>(h_[i]);
        // Be specific: serial + hub port + channel.
        *what = "setDeviceSerialNumber";
        if ((rc = Phidget_setDeviceSerialNumber(h, serial)) != EPHIDGET_OK) return rc;
        *what = "setHubPort";
        if ((rc = Phidget_setHubPort(h, hubPort)) != EPHIDGET_OK) return rc;
        *what = "setChannel";
        if ((rc = Phidget_setChannel(h, channel)) != EPHIDGET_OK) return rc;
        // Event handlers (registered before open so we catch the first attach).
        *what = "setOnAttachHandler";
        if ((rc = Phidget_setOnAttachHandler(h, OnAttachCb, ctx_[i].get())) != EPHIDGET_OK) return rc;
        *what = "setOnDetachHandler";
        if ((rc = Phidget_setOnDetachHandler(h, OnDetachCb, ctx_[i].get())) != EPHIDGET_OK) return rc;
        *what = "setOnErrorHandler";
        if ((rc = Phidget_setOnErrorHandler(h, OnErrorCb, ctx_[i].get())) != EPHIDGET_OK) return rc;
        return EPHIDGET_OK;
    }

    Rc OpenWaitForAttachment(int idx, uint32_t timeoutMs) override {
        return Phidget_openWaitForAttachment(Handle(idx), timeoutMs);
    }
    Rc Open(int idx) override { return Phidget_open(Handle(idx)); }
    Rc SetState(int idx, bool on) override {
        return PhidgetDigitalOutput_setState(h_[static_cast<size_t>(idx)], on ? 1 : 0);
    }
    Rc SetDutyCycle(int idx, double duty) override {
        return PhidgetDigitalOutput_setDutyCycle(h_[static_cast<size_t>(idx)], duty);
    }
    Rc EnableFailsafe(int idx, uint32_t ms) override {
        return PhidgetDigitalOutput_enableFailsafe(h_[static_cast<size_t>(idx)], ms);
    }
    Rc ResetFailsafe(int idx) override {
        return PhidgetDigitalOutput_resetFailsafe(h_[static_cast<size_t>(idx)]);
    }
    Rc Reopen(int idx) override {
        Phidget_close(Handle(idx));
        return Phidget_open(Handle(idx));
    }
    void Close(int idx) override {
        const size_t i = static_cast<size_t>(idx);
        if (i >= h_.size() || h_[i] == nullptr) return;
        Phidget_close(reinterpret_cast<PhidgetHandle>(h_[i]));
        PhidgetDigitalOutput_delete(&h_[i]);
    }

    const char* ErrStr(Rc rc) override {
        const char* desc = nullptr;
        Phidget_getErrorDescription(static_cast<PhidgetReturnCode>(rc), &desc);
        return desc ? desc : "?";
    }
    bool IsFailsafeRc(Rc rc) override { return rc == EPHIDGET_FAILSAFE; }

private:
    struct Ctx { HalListener* listener; int idx; };

    PhidgetHandle Handle(int idx) {
        return reinterpret_cast<PhidgetHandle>(h_[static_cast<size_t>(idx)]);
    }

    static void CCONV OnAttachCb(PhidgetHandle /*h*/, void* ctx) {
        auto* c = static_cast<Ctx*>(ctx);
        c->listener->OnAttach(c->idx);
    }
    static void CCONV OnDetachCb(PhidgetHandle /*h*/, void* ctx) {
        auto* c = static_cast<Ctx*>(ctx);
        c->listener->OnDetach(c->idx);
    }
    static void CCONV OnErrorCb(PhidgetHandle /*h*/, void* ctx, Phidget_ErrorEventCode code,
                                const char* description) {
        auto* c = static_cast<Ctx*>(ctx);
        c->listener->OnError(c->idx, static_cast<int>(code), description,
                             code == EEPHIDGET_FAILSAFE);
    }

    std::vector<PhidgetDigitalOutputHandle> h_;
    std::vector<std::unique_ptr<Ctx>> ctx_;   // stable addresses for the callbacks
};

} // namespace rc
