package utils

import (
	"sync"
)

type CLock struct {
	Segment [256]sync.RWMutex
}

func (l *CLock) Lock(key []byte) {
	lock := l.getLocker(key)
	lock.Lock()
}

func (l *CLock) RLock(key []byte) {
	lock := l.getLocker(key)
	lock.RLock()
}

func (l *CLock) RUnlock(key []byte) {
	lock := l.getLocker(key)
	lock.RUnlock()
}

func (l *CLock) Unlock(key []byte) {
	lock := l.getLocker(key)
	lock.Unlock()
}

func (l *CLock) getLocker(key []byte) *sync.RWMutex {
	number := GetHash256(key)
	return &l.Segment[number]
}

func NewCLock() *CLock {
	clock := CLock{}
	for i := 0; i < 256; i++ {
		clock.Segment[i] = sync.RWMutex{}
	}
	return &clock
}
