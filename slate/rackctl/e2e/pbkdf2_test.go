package e2e

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha512"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"strings"
	"testing"
)

// mosquittoHash produces the Mosquitto 2.x "$7$" PBKDF2-SHA512 format.
func mosquittoHash(pw string) string {
	salt := make([]byte, 12)
	rand.Read(salt)
	const iter = 101
	dk := pbkdf2SHA512([]byte(pw), salt, iter, 64)
	return fmt.Sprintf("$7$%d$%s$%s", iter, base64.StdEncoding.EncodeToString(salt), base64.StdEncoding.EncodeToString(dk))
}

func pbkdf2SHA512(pw, salt []byte, iter, keyLen int) []byte {
	var out []byte
	for block := uint32(1); len(out) < keyLen; block++ {
		mac := hmac.New(sha512.New, pw)
		mac.Write(salt)
		var bb [4]byte
		binary.BigEndian.PutUint32(bb[:], block)
		mac.Write(bb[:])
		u := mac.Sum(nil)
		x := append([]byte(nil), u...)
		for i := 1; i < iter; i++ {
			mac = hmac.New(sha512.New, pw)
			mac.Write(u)
			u = mac.Sum(nil)
			for j := range x {
				x[j] ^= u[j]
			}
		}
		out = append(out, x...)
	}
	return out[:keyLen]
}

// PBKDF2-HMAC-SHA512 known-answer test (password "password", salt "salt").
func TestPBKDF2SHA512(t *testing.T) {
	got := hex.EncodeToString(pbkdf2SHA512([]byte("password"), []byte("salt"), 1, 64))
	want := "867f70cf1ade02cff3752599a3a53dc4af34c7a669815ae5d513554e1c8cf252c02d470a285a0501bad999bfe943c08f050235d7d68b1da55e63f73b60a57fce"
	if got != want {
		t.Fatalf("got %s", got)
	}
	h := mosquittoHash("x")
	if p := strings.Split(h, "$"); len(p) != 5 || p[1] != "7" || p[2] != "101" {
		t.Fatalf("format %q", h)
	}
}
