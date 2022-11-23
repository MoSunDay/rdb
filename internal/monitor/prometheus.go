package monitor

import (
	"log"
	"net/http"
	"rdb/internal/utils"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

var confLogger = utils.GetLogger("monitor")

type CustomCollector struct {
	Latency    *prometheus.HistogramVec
	RaftStatus *prometheus.GaugeVec
}

func newCollector() *CustomCollector {
	c := &CustomCollector{
		Latency: prometheus.NewHistogramVec(prometheus.HistogramOpts{
			Name:    "rdb_command_latency",
			Help:    "rdb command latency(millisecond)",
			Buckets: prometheus.LinearBuckets(5, 25, 8),
		}, []string{"type"}),
		RaftStatus: prometheus.NewGaugeVec(prometheus.GaugeOpts{
			Name: "raft_stats",
			Help: "raft stats",
		}, []string{"type"}),
	}

	return c
}

func NewCustomCollector(bind string) *CustomCollector {
	c := newCollector()
	c.ListenAndServe(bind)
	return c
}

func (c *CustomCollector) ListenAndServe(bind string) {
	go func() {
		confLogger.Println("init monitor...")
		prometheus.MustRegister(c.Latency)
		// prometheus.MustRegister(c.RaftStatus)

		http.Handle("/metrics", promhttp.Handler())
		log.Fatal(http.ListenAndServe(bind, nil))
	}()
}
