package monitor

import (
	"log"
	"net/http"
	"rdb/internal/conf"
	"rdb/internal/utils"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

var confLogger = utils.GetLogger("monitor")
var (
	Collector = newCustomCollector()
)

type CustomCollector struct {
	QPS        *prometheus.CounterVec
	Latency    *prometheus.HistogramVec
	UP         *prometheus.Gauge
	RaftStatus *prometheus.GaugeVec
}

func newCustomCollector() *CustomCollector {
	return &CustomCollector{
		Latency: prometheus.NewHistogramVec(prometheus.HistogramOpts{
			Name:    "rdb_command_latency",
			Help:    "rdb command latency(millisecond)",
			Buckets: prometheus.LinearBuckets(5, 25, 8),
		}, []string{"type"}),
	}
}

func Setup() {
	go func() {
		confLogger.Println("init monitor...")
		prometheus.MustRegister(Collector.Latency)

		http.Handle("/metrics", promhttp.Handler())
		log.Fatal(http.ListenAndServe(conf.Content.MonitorAddr, nil))
	}()
}
