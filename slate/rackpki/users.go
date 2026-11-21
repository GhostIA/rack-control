package main

import (
	"fmt"
	"strings"
)

// Users is the fixed MQTT user list. It matches slate/mosquitto/acl, and
// the cert CN, the MQTT username and the client ID are all this name.
var Users = []string{
	"orchestrator",
	"sbc4",
	"intel-91c45d",
	"intel-d1babf",
	"intel-177e78",
	"intel-a163f8",
	"intel-1ecbc8",
}

// BrokerBundle is the pseudo-user name `bundle` uses for the broker's files.
const BrokerBundle = "broker"

func validUser(name string) error {
	for _, u := range Users {
		if u == name {
			return nil
		}
	}
	return fmt.Errorf("unknown user %q (valid: %s)", name, strings.Join(Users, ", "))
}

// PasswordEnvVar maps a username to its 1Password environment variable,
// e.g. intel-91c45d -> MQTT_INTEL_91C45D_PASSWORD.
func PasswordEnvVar(user string) string {
	return "MQTT_" + strings.ToUpper(strings.ReplaceAll(user, "-", "_")) + "_PASSWORD"
}

// passwordFor reads and validates a user's password from the environment.
// The returned error names the variable only, never its value.
func passwordFor(user string, getenv func(string) string) (string, error) {
	name := PasswordEnvVar(user)
	pw := getenv(name)
	if pw == "" {
		return "", fmt.Errorf("%s is missing or empty", name)
	}
	if strings.TrimSpace(pw) != pw {
		// Readers of <user>.password may or may not trim; refuse the ambiguity.
		return "", fmt.Errorf("%s has leading/trailing whitespace", name)
	}
	if strings.ContainsAny(pw, "\x00\r\n") {
		return "", fmt.Errorf("%s contains a NUL or newline character", name)
	}
	return pw, nil
}
