package main

import (
	"crypto/rand"
	"crypto/sha512"
	"encoding/base64"
	"errors"
	"fmt"
	"strings"

	"golang.org/x/crypto/pbkdf2"
)

// Mosquitto 2.x password_file format, checked against eclipse/mosquitto
// v2.0.20 src/password_mosq.{c,h}:
//
//	SALT_LEN 12, PW_DEFAULT_ITERATIONS 101, pw_sha512_pbkdf2 = 7,
//	PKCS5_PBKDF2_HMAC(password, strlen(password), salt, 12, iterations,
//	                  sha512, 64, hash)
//	base64 via BIO_f_base64 with BIO_FLAGS_BASE64_NO_NL (standard alphabet,
//	padded, no newlines)
//	line: "%s:$%d$%d$%s$%s\n" -> user:$7$101$<salt64>$<hash64>
const (
	pwHashType   = 7
	pwIterations = 101
	pwSaltLen    = 12
	pwKeyLen     = 64
)

func mosquittoHash(password string, salt []byte, iterations int) []byte {
	return pbkdf2.Key([]byte(password), salt, iterations, pwKeyLen, sha512.New)
}

// passwdLine returns one password_file line (no trailing newline) using a
// fresh random salt.
func passwdLine(user, password string) (string, error) {
	salt := make([]byte, pwSaltLen)
	if _, err := rand.Read(salt); err != nil {
		return "", err
	}
	return formatPasswdLine(user, password, salt), nil
}

func formatPasswdLine(user, password string, salt []byte) string {
	h := mosquittoHash(password, salt, pwIterations)
	return fmt.Sprintf("%s:$%d$%d$%s$%s", user, pwHashType, pwIterations,
		base64.StdEncoding.EncodeToString(salt), base64.StdEncoding.EncodeToString(h))
}

// buildPasswd renders the whole passwd file for every user in Users. It
// fails if any password variable is missing; the error lists every missing
// variable name, never a value.
func buildPasswd(getenv func(string) string) ([]byte, error) {
	var b strings.Builder
	var errs []error
	for _, u := range Users {
		pw, err := passwordFor(u, getenv)
		if err != nil {
			errs = append(errs, err)
			continue
		}
		line, err := passwdLine(u, pw)
		if err != nil {
			return nil, err
		}
		b.WriteString(line)
		b.WriteByte('\n')
	}
	if len(errs) > 0 {
		return nil, errors.Join(errs...)
	}
	return []byte(b.String()), nil
}
