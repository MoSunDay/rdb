package conf

import (
	"flag"
	"io/ioutil"
	"rdb/internal/utils"

	"gopkg.in/yaml.v2"
)

type Config struct {
	StorePath      string   `yaml:"store_path"`
	Bind           string   `yaml:"bind"`
	Instances      []string `yaml:"instances"`
	JoinAddress    string   `yaml:"raft_join_address"`
	RaftTCPAddress string   `yaml:"raft_bind_address"`
	Bootstrap      bool     `yaml:"raft_bootstrap"`
	HttpAddress    string   `yaml:"raft_http_bind_address"`
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
	confLogger.Printf("conf: %+v", Content)
}
