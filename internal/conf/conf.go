package conf

import (
	"flag"
	"io/ioutil"
	"rdb/internal/rcache"
	"rdb/internal/rtypes"
	"rdb/internal/utils"

	"gopkg.in/yaml.v2"
)

type Config struct {
	StorePath      string `yaml:"store_path"`
	Bind           string `yaml:"bind"`
	RaftTCPAddress string `yaml:"raft_bind_address"`
	HttpAddress    string `yaml:"raft_http_bind_address"`
	RaftToken      string `yaml:"raft_token"`
	MonitorAddr    string `yaml:"monitor_addr"`
	ClusterReady   bool
	Sentinel       rtypes.Sentinel
	MigrateTask    rtypes.MigrateTask
	CRaft          *rcache.Cached
	StableAddrs    []string
	BackupAddrs    []string
	PerNodeslots   int
	Helper         rtypes.Helper
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
}
