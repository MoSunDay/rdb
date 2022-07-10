package command

import (
	"fmt"
	"os"
	types "rdb/internal/rtypes"
	"strconv"
	"strings"
)

func clusterHandler(c types.CommandContext) {
	// conn, db, args := c.Conn, c.DB, c.Args
	args := c.Args
	subCommand := map[string]func(c types.CommandContext){
		"help":  clusterHelp,
		"info":  clusterInfo,
		"nodes": clusterNodes,
	}
	if len(args) == 0 {
		clusterHelp(c)
		return
	}
	if fn, ok := subCommand[string(args[0])]; ok {
		fn(c)
	} else {
		clusterHelp(c)
	}
}

func clusterInfo(c types.CommandContext) {
	conn := c.Conn
	clusterStatus := "ok"
	addrs := strings.Split(os.Args[2], ",")
	size := len(addrs)
	epoch := "1"
	conn.WriteBulkString(fmt.Sprintf(""+
		"cluster_state:%s\n"+
		"cluster_slots_assigned:16384\n"+
		"cluster_slots_ok:16384\n"+
		"cluster_slots_pfail:0\n"+
		"cluster_slots_fail:0\n"+
		"cluster_known_nodes:%d\n"+
		"cluster_size:%d\n"+
		"cluster_current_epoch:%s\n"+
		"cluster_my_epoch:%s\n"+
		"cluster_stats_messages_sent:0\n"+
		"cluster_stats_messages_received:0\n",
		clusterStatus, size, size, epoch, epoch,
	))
}

func clusterHelp(c types.CommandContext) {
	conn := c.Conn
	conn.WriteString("CLUSTER [ help | NODES | SLOTS | INIT ]")
}

func clusterNodes(c types.CommandContext) {
	conn := c.Conn
	addrs := strings.Split(os.Args[2], ",")
	nodeSlots := getNodeSlots()
	response := make([]string, len(addrs))
	for _, addr := range addrs {
		addrSlice := strings.Split(addr, ":")
		portStr := addrSlice[1]
		port, err := strconv.Atoi(portStr)
		if err != nil {
			conn.WriteError("cluster slots verification failed")
			return
		}
		portStr = strconv.Itoa(port + 200)
		addr = addrSlice[0] + ":" + portStr
		uuid := make([]byte, 40)

		for i := 0; i < 40; i++ {
			if i < len(addr) {
				if addr[i] != '.' && addr[i] != ':' {
					uuid[i] = addr[i]
				} else {
					uuid[i] = 'a'
				}
			} else {
				uuid[i] = 'b'
			}
		}

		nodeInfo := fmt.Sprintf("%s %s@%s myself,master - 0 0 connected %s\r\n", string(uuid), addr, portStr, nodeSlots[addr])
		response = append(response, nodeInfo)
	}
	conn.WriteBulkString(strings.Join(response, ""))
}

func getNodeSlots() map[string]string {
	nodeSlots := make(map[string]string)
	addrs := strings.Split(os.Args[2], ",")
	slotNumber := 16384
	perNodeslots := slotNumber / len(addrs)

	start, end := 0, 0
	for index, addr := range addrs {
		if index == len(addrs)-1 {
			nodeSlots[addr] = fmt.Sprintf("%d-%d", end+1, slotNumber-1)
		} else {
			end = perNodeslots * (index + 1)
			nodeSlots[addr] = fmt.Sprintf("%d-%d", start, end)
			start += end
			start += 1
		}
	}
	fmt.Println(nodeSlots)
	return nodeSlots
}

// func clusterSlots(c types.CommandContext) {
// 	c.WriteArray(1)
// 	c.WriteArray(3)
// 	c.WriteInt64(0)
// 	c.WriteInt64(16383)
// 	c.WriteArray(3)
// 	c.WriteBulkString("0.0.0.0")
// 	c.WriteInt64(6666)
// 	c.WriteBulkString("356a192b7913b04c54574d18c28d46e6395428ab")
// }

// func clusterTest(c types.CommandContext) {
// 	c.WriteString("-MOVED 0 0.0.0.0:6666")
// }
