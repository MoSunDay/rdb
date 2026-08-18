package utils

import (
	"bytes"
	"unsafe"
)

func BytesCombine(pBytes ...[]byte) []byte {
	return bytes.Join(pBytes, []byte(""))
}

func BytesToString(buf []byte) string {
	return *(*string)(unsafe.Pointer(&buf))
}

func StringSliceReplaceItem(slice []string, src, target string) []string {
	for i := 0; i < len(slice); i++ {
		if slice[i] == src {
			slice[i] = target
		}
	}
	return slice
}
