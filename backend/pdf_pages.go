package main

import (
	"bytes"
	"compress/zlib"
	"io"
	"os"
	"regexp"
	"strconv"
)

// pdfPageCountMaxSize bounds the PDFs whose pages get counted, since the
// whole file is read into memory.
const pdfPageCountMaxSize = 64 << 20

var (
	pdfPagesRe   = regexp.MustCompile(`/Type\s*/Pages\b`)
	pdfCountRe   = regexp.MustCompile(`/Count\s+(\d+)`)
	pdfParentRe  = regexp.MustCompile(`/Parent\s+\d+\s+\d+\s+R`)
	pdfObjStmRe  = regexp.MustCompile(`(?s)/Type\s*/ObjStm\b.*?stream\r?\n`)
	pdfEncryptRe = regexp.MustCompile(`/Encrypt\s+\d+\s+\d+\s+R`)
)

// pdfPageCount counts the pages of a PDF for the document message's
// pageCount, which WA Web fills in only for PDFs (WAWebDocumentUploadEnrichment).
// It reads the root page tree's /Count, looking inside compressed object
// streams too. It returns 0 when the count can't be found, such as for an
// encrypted file.
func pdfPageCount(path string) uint32 {
	info, err := os.Stat(path)
	if err != nil || info.Size() > pdfPageCountMaxSize {
		return 0
	}
	data, err := os.ReadFile(path)
	if err != nil || !bytes.HasPrefix(data, []byte("%PDF-")) {
		return 0
	}
	if pdfEncryptRe.Match(data) {
		return 0
	}
	best := pdfRootPageCount(data)
	for _, loc := range pdfObjStmRe.FindAllIndex(data, -1) {
		r, err := zlib.NewReader(bytes.NewReader(data[loc[1]:]))
		if err != nil {
			continue
		}
		inflated, _ := io.ReadAll(io.LimitReader(r, pdfPageCountMaxSize))
		r.Close()
		best = max(best, pdfRootPageCount(inflated))
	}
	return best
}

// pdfRootPageCount finds the /Count of a /Pages dictionary without a
// /Parent, the root of the page tree.
func pdfRootPageCount(data []byte) uint32 {
	var best uint32
	for _, loc := range pdfPagesRe.FindAllIndex(data, -1) {
		dict := pdfDictTopLevel(data, loc[0])
		if dict == nil || !pdfPagesRe.Match(dict) || pdfParentRe.Match(dict) {
			continue
		}
		m := pdfCountRe.FindSubmatch(dict)
		if m == nil {
			continue
		}
		if n, err := strconv.ParseUint(string(m[1]), 10, 32); err == nil && uint32(n) > best {
			best = uint32(n)
		}
	}
	return best
}

// pdfDictTopLevel returns the dictionary around pos with any nested
// dictionaries left out, so a nested /Count or /Parent isn't mistaken for
// this one's.
func pdfDictTopLevel(data []byte, pos int) []byte {
	start, depth := -1, 0
	for i := pos - 1; i > 0; i-- {
		if data[i-1] == '>' && data[i] == '>' {
			depth++
			i--
		} else if data[i-1] == '<' && data[i] == '<' {
			if depth == 0 {
				start = i - 1
				break
			}
			depth--
			i--
		}
	}
	if start < 0 {
		return nil
	}
	var out []byte
	depth = 0
	for i := start; i+1 < len(data); i++ {
		switch {
		case data[i] == '<' && data[i+1] == '<':
			depth++
			i++
			continue
		case data[i] == '>' && data[i+1] == '>':
			depth--
			i++
			if depth == 0 {
				return out
			}
			continue
		}
		if depth == 1 {
			out = append(out, data[i])
		}
	}
	return nil
}
