package utils

import (
	"crypto/md5"
	"encoding/hex"
	"os"
)

func GetEnvDefault(key, defVal string) string {
	val, ex := os.LookupEnv(key)
	if !ex {
		return defVal
	}
	return val
}

func MD5With40(str string) string {
	h := md5.New()
	h.Write([]byte(str))
	result := hex.EncodeToString(h.Sum(nil))
	return result + result[24:]
}
