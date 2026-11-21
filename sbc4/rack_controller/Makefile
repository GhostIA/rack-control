# rack_controller
#   make            the real build, ON THE SBC4 only (needs libphidget22)
#   make test       unit tests on the dev box (fake HAL, real AF_UNIX socket)
#   make check-mac  syntax-check the Phidget code against Phidget22.framework
#   make xcheck     relay-mqtt's own client + parser against this socket server
#                   (needs cargo; builds offline from the local cargo cache)
#   make install    copy the binary into /usr/userapps/rack_controller on the SBC4

CXX       ?= g++
CXXFLAGS  ?= -O2
WARN      := -Wall -Wextra
SRCS      := rack_controller.cpp core.cpp
HDRS      := core.h hal.h hal_phidget.h

TEST_CXX  := clang++
TEST_FLAGS:= -std=c++17 -Wall -Wextra -Werror -g -pthread
BUILD     := bin

PHIDGET_FW := /Library/Frameworks
PREFIX     := /usr/userapps/rack_controller
UNITDIR    := /etc/systemd/system

.PHONY: all test check-mac xcheck install clean

all: rack_controller

rack_controller: $(SRCS) $(HDRS)
	$(CXX) -std=c++17 $(CXXFLAGS) $(WARN) $(SRCS) -lphidget22 -lpthread -o $@

$(BUILD)/unit_tests: core.cpp core.h hal.h tests/test_main.cpp tests/fake_hal.h
	@mkdir -p $(BUILD)
	$(TEST_CXX) $(TEST_FLAGS) core.cpp tests/test_main.cpp -o $@

$(BUILD)/test_server: core.cpp core.h hal.h tests/test_server.cpp tests/fake_hal.h
	@mkdir -p $(BUILD)
	$(TEST_CXX) $(TEST_FLAGS) core.cpp tests/test_server.cpp -o $@

test: $(BUILD)/unit_tests $(BUILD)/test_server
	./$(BUILD)/unit_tests

check-mac:
	clang++ -std=c++17 -Wall -Wextra -Werror -fsyntax-only -F$(PHIDGET_FW) \
		-I$(PHIDGET_FW)/Phidget22.framework/Headers $(SRCS)

xcheck: $(BUILD)/test_server
	RC_TEST_SERVER="$(CURDIR)/$(BUILD)/test_server" \
		cargo test --offline --manifest-path tests/client_xcheck/Cargo.toml

install: rack_controller
	install -d $(PREFIX)
	install -m 0755 rack_controller $(PREFIX)/rack_controller

clean:
	rm -rf $(BUILD) rack_controller tests/client_xcheck/target
