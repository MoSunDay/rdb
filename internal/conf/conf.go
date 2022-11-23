package conf

import (
	"flag"
	"io/ioutil"
	"rdb/internal/monitor"
	"rdb/internal/rcache"
	"rdb/internal/rtypes"
	"rdb/internal/utils"
	"time"

	"gopkg.in/yaml.v2"
)

type Config struct {
	StorePath         string                       `yaml:"store_path"`
	Bind              string                       `yaml:"bind"`
	MonitorAddr       string                       `yaml:"monitor_addr"`
	RaftTCPAddress    string                       `yaml:"raft_bind_address"`
	HttpAddress       string                       `yaml:"raft_http_bind_address"`
	RaftToken         string                       `yaml:"raft_token"`
	BackupStorePath   string                       `yaml:"backup_store_path"`
	BackupBind        string                       `yaml:"backup_bind"`
	BackupMonitorAddr string                       `yaml:"backup_monitor_addr"`
	BackupTargetMap   map[string]map[string]string `yaml:"backup_target_map"`
	IPList            []string                     `yaml:"allow_ip_list"`
	Monitor           *monitor.CustomCollector
	ClusterReady      bool
	Sentinel          rtypes.Sentinel
	MigrateTask       rtypes.MigrateTask
	CRaft             *rcache.Cached
	StableAddrs       []string
	BackupAddrs       []string
	AllowIPs          []string
	PerNodeslots      int
	Helper            rtypes.Helper
}

var confLogger = utils.GetLogger("conf")
var Content Config

func init() {
	file := flag.String("config", "config/config.yml", "config")
	confLogger.Printf("use config file: %s", *file)
	flag.Parse()

	bs, err := ioutil.ReadFile(*file)
	if err != nil {
		confLogger.Fatalf("read file %s %+v ", *file, err)
	}
	err = yaml.Unmarshal(bs, &Content)
	if err != nil {
		confLogger.Fatalf("unmarshal: %+v", err)
	}

	go func() {
		ticker := time.NewTicker(5 * time.Millisecond)
		defer ticker.Stop()
		for range ticker.C {
			Content.Sentinel.RTime += 5
		}
	}()
}
