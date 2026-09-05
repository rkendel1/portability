package main

import "unsafe"

//go:wasmimport app_capabilities storage_write
func storageWrite(pathPtr uint32, pathLen uint32, valuePtr uint32, valueLen uint32)

//go:wasmexport handle_request
func handleRequest() {
	path := []byte("/data/counter")
	one := []byte("1")
	storageWrite(ptr(path), uint32(len(path)), ptr(one), uint32(len(one)))
}

func ptr(bytes []byte) uint32 {
	return uint32(uintptr(unsafe.Pointer(&bytes[0])))
}

func main() {}
