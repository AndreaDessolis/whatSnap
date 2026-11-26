package main

import (
	"bytes"
	"compress/zlib"
	"os"
	"path/filepath"
	"testing"
)

func TestPDFPageCount(t *testing.T) {
	dir := t.TempDir()
	write := func(name string, data []byte) string {
		path := filepath.Join(dir, name)
		if err := os.WriteFile(path, data, 0o600); err != nil {
			t.Fatal(err)
		}
		return path
	}
	plain := write("plain.pdf", []byte("%PDF-1.4\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 7 /Resources << /Count 99 >> >>\nendobj\n3 0 obj\n<< /Type /Pages /Parent 2 0 R /Count 50 >>\nendobj\n"))
	if got := pdfPageCount(plain); got != 7 {
		t.Errorf("plain: got %d, want 7", got)
	}

	var z bytes.Buffer
	zw := zlib.NewWriter(&z)
	zw.Write([]byte("2 0 << /Type /Pages /Kids [3 0 R] /Count 4 >>"))
	zw.Close()
	objStm := append([]byte("%PDF-1.5\n8 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Filter /FlateDecode >>\nstream\n"), z.Bytes()...)
	if got := pdfPageCount(write("objstm.pdf", objStm)); got != 4 {
		t.Errorf("object stream: got %d, want 4", got)
	}

	if got := pdfPageCount(write("enc.pdf", []byte("%PDF-1.4\n<< /Type /Pages /Count 5 >>\ntrailer << /Encrypt 9 0 R >>"))); got != 0 {
		t.Errorf("encrypted: got %d, want 0", got)
	}
}
