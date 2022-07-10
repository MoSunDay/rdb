package conf

import (
	"flag"
	"io/ioutil"
	"rdb/internal/utils"

	"gopkg.in/yaml.v2"
)

type Store struct {
	Engine string `yaml:"engine"`
	Path   string `yaml:"path"`
}

type Config struct {
	Store   Store   `yaml:"store"`
	Server  Server  `yaml:"server"`
	Cluster Cluster `yaml:"cluster"`
}

type Server struct {
	ServerPort int `yaml:"Server_port"`
	HttpPort   int `yaml:"http_port"`
}

type Cluster struct {
	Path    string `yaml:"path"`
	Address string `yaml:"address"`
}

var confLogger = utils.GetLogger("conf")
var Conf Config

func init() {
	file := flag.String("config", "configs/config.yml", "config")

	confLogger.Printf("use config file: %s", *file)

	flag.Parse()

	bs, err := ioutil.ReadFile(*file)
	if err != nil {
		confLogger.Fatalf("read file %s %+v ", *file, err)
	}
	err = yaml.Unmarshal(bs, &Conf)
	if err != nil {
		confLogger.Fatalf("unmarshal: %+v", err)
	}

	confLogger.Printf("conf: %+v", Conf)
}
