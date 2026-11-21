package main

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
)

const (
	modeSecret = 0o600 // keys, passwd, passwords, every deploy bundle file
	modePublic = 0o644 // certificates in the PKI dir
	modeDir    = 0o700
)

// outFile is one file a command wants to write.
type outFile struct {
	path string
	data []byte
	mode fs.FileMode
}

// writeAll writes files as one batch. Without force it first checks that no
// target exists, so a refused run leaves nothing written. Every file is
// first written in full to a temp file next to its target, with its final
// mode. Only when all temp files are written are they renamed into place,
// so an I/O error (e.g. disk full) never leaves a new cert next to an old key.
func writeAll(files []outFile, force bool) error {
	if !force {
		for _, f := range files {
			if _, err := os.Lstat(f.path); err == nil {
				return fmt.Errorf("%s already exists (use --force to overwrite)", f.path)
			} else if !errors.Is(err, fs.ErrNotExist) {
				return err
			}
		}
	}
	var temps []string
	defer func() {
		for _, t := range temps {
			os.Remove(t) // no-op for temps already renamed
		}
	}()
	for _, f := range files {
		if err := os.MkdirAll(filepath.Dir(f.path), modeDir); err != nil {
			return err
		}
		tmp, err := writeTemp(f.path, f.data, f.mode)
		if err != nil {
			return err
		}
		temps = append(temps, tmp)
	}
	for i, f := range files {
		if err := os.Rename(temps[i], f.path); err != nil {
			return err
		}
	}
	return nil
}

// writeTemp writes data in full to a new temp file in path's directory, with
// the given mode, and returns its name.
func writeTemp(path string, data []byte, mode fs.FileMode) (string, error) {
	tmp, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+".tmp-*")
	if err != nil {
		return "", err
	}
	name := tmp.Name()
	err = func() error {
		// CreateTemp makes the file 0600; Chmod sets the exact mode, umask aside.
		if err := tmp.Chmod(mode); err != nil {
			return err
		}
		if _, err := tmp.Write(data); err != nil {
			return err
		}
		return tmp.Sync()
	}()
	if cerr := tmp.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		os.Remove(name)
		return "", err
	}
	return name, nil
}

// findRepoRoot walks up from dir to the rack-control checkout (the first
// directory containing slate/mosquitto/acl).
func findRepoRoot(dir string) (string, error) {
	dir, err := filepath.Abs(dir)
	if err != nil {
		return "", err
	}
	for {
		if st, err := os.Stat(filepath.Join(dir, "slate", "mosquitto", "acl")); err == nil && st.Mode().IsRegular() {
			return dir, nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", errors.New("cannot find the rack-control repo (slate/mosquitto/acl) above the current directory; pass --out and --mosquitto-dir")
		}
		dir = parent
	}
}
