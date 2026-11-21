// fake_hal.h -- in-memory rc::Hal for tests. Records every call, models output
// state / failsafe arming per channel, and can inject failures and events.
#pragma once

#include "../hal.h"

#include <atomic>
#include <chrono>
#include <cstdio>
#include <map>
#include <thread>
#include <mutex>
#include <set>
#include <string>
#include <vector>

namespace rc {

constexpr Rc kFakeFail = 7;       // generic failure
constexpr Rc kFakeFailsafe = 59;  // mirrors EPHIDGET_FAILSAFE (0x3b)

class FakeHal : public Hal {
public:
    struct Out {
        bool created = false, opened = false, closed = false;
        bool state = false;
        double duty = -1.0;
        bool failsafe = false;
        uint32_t failsafeMs = 0;
        int resets = 0;
        int enables = 0;
    };

    // Configuration (set before use).
    bool attachOnBlockingOpen = true;   // OpenWaitForAttachment fires OnAttach
    bool attachOnOpen = false;          // Open fires OnAttach immediately
    std::set<int> failOpenWait;         // idx whose blocking open times out
    std::set<int> failSetState;         // idx whose SetState fails
    std::set<int> failDuty;             // idx whose SetDutyCycle fails
    std::set<int> failEnable;           // idx whose EnableFailsafe fails
    std::set<int> failsafeSetState;     // idx whose SetState returns the failsafe rc
    std::atomic<bool> holdDuty{false};  // SetDutyCycle blocks while true (a slow device)

    Rc Create(int idx, int, int, int, HalListener* l, const char** what) override {
        std::lock_guard<std::mutex> lk(mu_);
        listener_ = l;
        out_[idx].created = true;
        *what = "create";
        Log("create " + std::to_string(idx));
        return kOk;
    }
    Rc OpenWaitForAttachment(int idx, uint32_t) override {
        {
            std::lock_guard<std::mutex> lk(mu_);
            Log("openwait " + std::to_string(idx));
            if (failOpenWait.count(idx)) return kFakeFail;
            out_[idx].opened = true;
        }
        if (attachOnBlockingOpen) listener_->OnAttach(idx);
        return kOk;
    }
    Rc Open(int idx) override {
        {
            std::lock_guard<std::mutex> lk(mu_);
            Log("open " + std::to_string(idx));
            out_[idx].opened = true;
        }
        if (attachOnOpen) listener_->OnAttach(idx);
        return kOk;
    }
    Rc SetState(int idx, bool on) override {
        std::lock_guard<std::mutex> lk(mu_);
        Log("setstate " + std::to_string(idx) + " " + (on ? "1" : "0"));
        if (failsafeSetState.count(idx)) return kFakeFailsafe;
        if (failSetState.count(idx)) return kFakeFail;
        out_[idx].state = on;
        return kOk;
    }
    Rc SetDutyCycle(int idx, double d) override {
        while (holdDuty.load()) std::this_thread::sleep_for(std::chrono::milliseconds(1));
        std::lock_guard<std::mutex> lk(mu_);
        char b[48];
        std::snprintf(b, sizeof b, "duty %d %.2f", idx, d);
        Log(b);
        if (failDuty.count(idx)) return kFakeFail;
        out_[idx].duty = d;
        return kOk;
    }
    Rc EnableFailsafe(int idx, uint32_t ms) override {
        std::lock_guard<std::mutex> lk(mu_);
        Log("enable " + std::to_string(idx));
        if (failEnable.count(idx)) return kFakeFail;
        out_[idx].failsafe = true;
        out_[idx].failsafeMs = ms;
        out_[idx].enables++;
        return kOk;
    }
    Rc ResetFailsafe(int idx) override {
        std::lock_guard<std::mutex> lk(mu_);
        out_[idx].resets++;
        return out_[idx].failsafe ? kOk : kFakeFail;
    }
    Rc Reopen(int idx) override {
        {
            std::lock_guard<std::mutex> lk(mu_);
            Log("reopen " + std::to_string(idx));
            out_[idx].failsafe = false;
            out_[idx].state = false;
        }
        if (attachOnOpen) listener_->OnAttach(idx);
        return kOk;
    }
    void Close(int idx) override {
        std::lock_guard<std::mutex> lk(mu_);
        Log("close " + std::to_string(idx));
        out_[idx].closed = true;
    }
    const char* ErrStr(Rc rc) override { return rc == kFakeFailsafe ? "failsafe" : "fake failure"; }
    bool IsFailsafeRc(Rc rc) override { return rc == kFakeFailsafe; }

    // ---- Test helpers ----
    void FireAttach(int idx) { listener_->OnAttach(idx); }
    void FireDetach(int idx) {
        {
            std::lock_guard<std::mutex> lk(mu_);
            out_[idx].failsafe = false;
            out_[idx].state = false;
        }
        listener_->OnDetach(idx);
    }
    void FireTrip(int idx) {
        {
            std::lock_guard<std::mutex> lk(mu_);
            out_[idx].state = false;
        }
        listener_->OnError(idx, 0x100c, "Failsafe Triggered", true);
    }
    Out Get(int idx) {
        std::lock_guard<std::mutex> lk(mu_);
        return out_[idx];
    }
    std::vector<std::string> Calls() {
        std::lock_guard<std::mutex> lk(mu_);
        return calls_;
    }
    void ClearCalls() {
        std::lock_guard<std::mutex> lk(mu_);
        calls_.clear();
    }

private:
    void Log(const std::string& s) { calls_.push_back(s); }

    std::mutex mu_;
    HalListener* listener_ = nullptr;
    std::map<int, Out> out_;
    std::vector<std::string> calls_;
};

} // namespace rc
