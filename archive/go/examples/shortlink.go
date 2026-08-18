package main

import (
	"context"
	"fmt"
	"log"
	"math"
	"math/rand"
	"net/http"
	"strings"
	"time"

	redis "github.com/go-redis/redis/v8"
	mm3 "github.com/spaolacci/murmur3"
)

var tplContent = `
<html>
<head>
<title></title>
<link rel="stylesheet" href="https://maxcdn.bootstrapcdn.com/bootstrap/4.0.0/css/bootstrap.min.css" integrity="sha384-Gn5384xqQ1aoWXA+058RXPxPg6fy4IWvTNh0E263XmFcJlSAwiGgFAW/dAiS6JXm" crossorigin="anonymous">
<script src="https://cdnjs.cloudflare.com/ajax/libs/popper.js/1.12.9/umd/popper.min.js" integrity="sha384-ApNbgh9B+Y1QKtv3Rn7W3mgPxhU9K/ScQsAP7hUibX39j7fakFPskvXusvfa0b4Q" crossorigin="anonymous"></script>
<script src="https://maxcdn.bootstrapcdn.com/bootstrap/4.0.0/js/bootstrap.min.js" integrity="sha384-JZR6Spejh4U02d8jOt6vLEHfe/JQGiRRSQQxSfFWpi1MquVdAyjUar5+76PVCmYl" crossorigin="anonymous"></script>
<script src="https://cdn.jsdelivr.net/npm/clipboard@2.0.8/dist/clipboard.min.js"></script>

<style>
.card {
	margin: 30px;
  }
</style>
</head>
<body>
<div class="card">
  <div class="card-body">
  <form action="/s" method="get" class="form-inline">
  <div class="form-group mx-sm-3">
    <input type="url" name="s" class="form-control" id="url" placeholder="url" style="width: 600px;">
  </div>
  <button type="submit" class="btn btn-primary">生成短链</button>
  </form>
  </div>
</div>
<div class="card">
<div class="list-group">
  <a href="/e" class="list-group-item list-group-item-action">我想把长文本的内容变短链「点我！」</a>
  <a href="/n" class="list-group-item list-group-item-action">我想给长链接取一个好记的名字「点我！」</a>
</div>
</div>
</body>
</html>
`

var nameContent = `
<html>
<head>
<title></title>
<link rel="stylesheet" href="https://maxcdn.bootstrapcdn.com/bootstrap/4.0.0/css/bootstrap.min.css" integrity="sha384-Gn5384xqQ1aoWXA+058RXPxPg6fy4IWvTNh0E263XmFcJlSAwiGgFAW/dAiS6JXm" crossorigin="anonymous">
<script src="https://cdnjs.cloudflare.com/ajax/libs/popper.js/1.12.9/umd/popper.min.js" integrity="sha384-ApNbgh9B+Y1QKtv3Rn7W3mgPxhU9K/ScQsAP7hUibX39j7fakFPskvXusvfa0b4Q" crossorigin="anonymous"></script>
<script src="https://maxcdn.bootstrapcdn.com/bootstrap/4.0.0/js/bootstrap.min.js" integrity="sha384-JZR6Spejh4U02d8jOt6vLEHfe/JQGiRRSQQxSfFWpi1MquVdAyjUar5+76PVCmYl" crossorigin="anonymous"></script>
<script src="https://cdn.jsdelivr.net/npm/clipboard@2.0.8/dist/clipboard.min.js"></script>
<style>
.card {
	margin: 30px;
  }
</style>
</head>
<body>
<div class="card">
  <div class="card-body">
  <form action="/r" method="get">
  <div class="form-group mx-sm-3">
    <input type="url" name="s" class="form-control" id="url" placeholder="长连接" style="width: 600px;">
  </div>
  <div class="form-group mx-sm-3">
    <input name="d" class="form-control" id="url" placeholder="短连接 ID" style="width: 600px;">
  </div>
  <div class="form-group mx-sm-3">
    <button type="submit" class="btn btn-primary">提交</button>
  </div>
  </form>
  </div>
</div>
</body>
</html>
`
var formatContent = `
<html>
  <head>
    <title></title>
	<script src="https://cdnjs.cloudflare.com/ajax/libs/jquery/3.2.1/jquery.min.js"></script>
	<link href="https://cdnjs.cloudflare.com/ajax/libs/highlight.js/9.12.0/styles/default.min.css" rel="stylesheet" />
	<script src="https://cdnjs.cloudflare.com/ajax/libs/highlight.js/9.12.0/highlight.min.js"></script>
	<script type="text/javascript">
	  $(document).ready(function() {
		$('pre code').each(function(i, block) {
			hljs.highlightBlock(block)
			hljs.lineNumbersBlock(block,{
				singleLine: true      //开启单行行号显示
			});
		});
	  });
	</script>
  </head>
  <body>
  <pre><code>{value}</code></pre>
  </body>
</html>
`

var shortContent = `
<html>
  <head>
    <title></title>
	<link rel="stylesheet" href="https://maxcdn.bootstrapcdn.com/bootstrap/4.0.0/css/bootstrap.min.css"></link>
	<script src="https://stackpath.bootstrapcdn.com/bootstrap/4.0.0/js/bootstrap.bundle.min.js"></script>
	<script src="https://cdnjs.cloudflare.com/ajax/libs/jquery/3.2.1/jquery.min.js"></script>
    <script src="https://cdn.jsdelivr.net/npm/clipboard@2.0.8/dist/clipboard.min.js"></script>
    <script src="https://cdnjs.cloudflare.com/ajax/libs/clipboard.js/2.0.0/clipboard.min.js"></script>
	<script src="https://cdnjs.cloudflare.com/ajax/libs/jquery/3.2.1/jquery.min.js"></script>
	<script type="text/javascript">
	  $(document).ready(function() {
	    new ClipboardJS('.btn');
	  });
	</script>
	<style>
	.page-content {
		margin: 30px;
	}
</style>
  </head>
  <body>
    <div class="page-content page-container" id="page-content">
      <div class="padding">
        <div class="row container d-flex justify-content-center">
          <div class="col-12 grid-margin">
            <div class="card">
              <div class="row">
                <div class="col-md-6">
                  <div class="card-body">
                    <p class="card-description">点击 Copy 复制短链内容到粘贴板</p>
                    <input type="text" id="clipboardExample1" class="form-control" value="{value}">
                    <div class="mt-3"> <button type="button" class="btn btn-info btn-clipboard" data-clipboard-action="copy" data-clipboard-target="#clipboardExample1">Copy</button></div>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  </body>
</html>
`

var textContent = `
<html>
<head>
<title></title>
<link rel="stylesheet" href="https://maxcdn.bootstrapcdn.com/bootstrap/4.0.0/css/bootstrap.min.css" integrity="sha384-Gn5384xqQ1aoWXA+058RXPxPg6fy4IWvTNh0E263XmFcJlSAwiGgFAW/dAiS6JXm" crossorigin="anonymous">
<script src="https://cdnjs.cloudflare.com/ajax/libs/popper.js/1.12.9/umd/popper.min.js" integrity="sha384-ApNbgh9B+Y1QKtv3Rn7W3mgPxhU9K/ScQsAP7hUibX39j7fakFPskvXusvfa0b4Q" crossorigin="anonymous"></script>
<script src="https://maxcdn.bootstrapcdn.com/bootstrap/4.0.0/js/bootstrap.min.js" integrity="sha384-JZR6Spejh4U02d8jOt6vLEHfe/JQGiRRSQQxSfFWpi1MquVdAyjUar5+76PVCmYl" crossorigin="anonymous"></script>
<script src="https://cdn.jsdelivr.net/npm/clipboard@2.0.8/dist/clipboard.min.js"></script>
<style>
.card {
	margin: 30px;
  }
 textarea {
	overflow: scroll;
    min-height: 700px;
}
</style>
</head>
<body>
<div class="card">
  <div class="card-body">
  <form action="/s" method="get">
  <div class="form-group mx-sm-3">
    <textarea type="text" name="s" class="form-control" id="text" placeholder="text" style="width: 1000px;"></textarea>
  </div>
  <button type="submit" class="btn btn-primary" style="margin-left: 14px">生成短链</button>
  </form>
  </div>
</div>
</body>
</html>
`

var ctx = context.Background()

func init() {
	rand.Seed(time.Now().UnixNano())
}

var chars string = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"

func encode(num int64) string {
	bytes := []byte{}
	for num > 0 {
		bytes = append(bytes, chars[num%62])
		num = num / 62
	}
	reverse(bytes)
	return string(bytes)
}

func decode(str string) int64 {
	var num int64
	n := len(str)
	for i := 0; i < n; i++ {
		pos := strings.IndexByte(chars, str[i])
		num += int64(math.Pow(62, float64(n-i-1)) * float64(pos))
	}
	return num
}

func reverse(a []byte) {
	for left, right := 0, len(a)-1; left < right; left, right = left+1, right-1 {
		a[left], a[right] = a[right], a[left]
	}
}

func index(w http.ResponseWriter, r *http.Request) {
	if r.Method == "GET" {
		fmt.Fprintf(w, strings.ReplaceAll(tplContent, "{value}", r.Host))
	} else {
		fmt.Println("url:", r.Form)
	}
}

func main() {
	rdb := redis.NewClusterClient(&redis.ClusterOptions{
		Addrs: []string{"127.0.0.1:32680"},
	})

	http.HandleFunc("/index", index)
	http.HandleFunc("/s", func(w http.ResponseWriter, r *http.Request) {
		keys, ok := r.URL.Query()["s"]
		if !ok || len(keys[0]) < 1 {
			fmt.Fprintf(w, "Url Param 's' is missing")
			return
		}
		shortName := encode(int64(mm3.Sum32([]byte(keys[0]))))
		_, err := rdb.Set(ctx, shortName, keys[0], 0).Result()
		if err != nil {
			fmt.Fprintf(w, "Something went wrong please contact SRE")
			return
		}
		fmt.Fprintln(w, strings.Replace(shortContent, "{value}", "http://"+r.Host+"/"+shortName, 1))
	})
	http.HandleFunc("/e", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintf(w, textContent)
	})
	http.HandleFunc("/n", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintf(w, nameContent)
	})
	http.HandleFunc("/r", func(w http.ResponseWriter, r *http.Request) {
		sKeys, sOk := r.URL.Query()["s"]
		dKeys, dOk := r.URL.Query()["d"]
		if !sOk || !dOk || len(sKeys[0]) < 1 || len(dKeys[0]) < 1 {
			fmt.Fprintf(w, "Url Param 's' or 'd' is missing")
			return
		} else {
			shortName := dKeys[0]
			val, err := rdb.Get(ctx, shortName).Result()
			if val != "" || err == nil {
				fmt.Fprintf(w, shortName+" already exists")
				return
			}
			_, err = rdb.Set(ctx, shortName, sKeys[0], 0).Result()
			if err != nil {
				fmt.Fprintf(w, "Something went wrong please contact SRE")
				return
			}
			fmt.Fprintln(w, strings.Replace(shortContent, "{value}", "http://"+r.Host+"/"+shortName, 1))
		}
	})
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		var val string
		shortName := r.URL.Path[1:]
		if shortName == "" {
			http.Redirect(w, r, "http://"+r.Host+"/index", 302)
			return
		}
		val, err := rdb.Get(ctx, shortName).Result()
		if val == "" || err != nil {
			fmt.Fprintf(w, "No matching url found")
			return
		}
		if strings.HasPrefix(val, "http") {
			http.Redirect(w, r, val, 302)
			return
		}
		// fmt.Fprintln(w, val)
		fmt.Fprintln(w, strings.Replace(formatContent, "{value}", val, 1))
		return
	})
	err := http.ListenAndServe("0.0.0.0:80", nil)
	if err != nil {
		log.Fatal("ListenAndServe: ", err)
	}
}
