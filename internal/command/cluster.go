package command

import (
	"fmt"
	"rdb/internal/conf"
	types "rdb/internal/rtypes"
	"rdb/internal/utils"
	"strconv"
	"strings"
	"time"
)

func clusterHandler(c types.CommandContext) {
	// conn, db, args := c.Conn, c.DB, c.Args
	args := c.Args
	subCommand := map[string]func(c types.CommandContext){
		"help":  clusterHelp,
		"info":  clusterInfo,
		"INFO":  clusterInfo,
		"nodes": clusterNodes,
		"NODES": clusterNodes,
		"slots": clusterSlots,
		"SLOTS": clusterSlots,
		"test":  clusterTest,
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
	addrs := conf.Content.Instances
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
	conn.WriteString("CLUSTER [ help | nodes | slots | test ]")
}

func clusterNodes(c types.CommandContext) {
	conn := c.Conn
	addrs := conf.Content.Instances
	nodeSlots := getNodeSlots()
	response := make([]string, len(addrs))

	for _, addr := range addrs {
		addrSlice := strings.Split(addr, ":")
		portStr := addrSlice[1]
		uuid := utils.MD5With40(addr)
		var flag string
		if addr == conf.Content.Bind {
			flag = "myself,"
		} else {
			flag = ""
		}
		timestamp := time.Now().UnixMilli()

		nodeInfo := fmt.Sprintf("%s %s@%s %smaster - 0 %d 1 connected %s\r\n", uuid, addr, portStr, flag, timestamp, nodeSlots[addr])
		response = append(response, nodeInfo)
	}
	conn.WriteBulkString(strings.Join(response, ""))
}

func getNodeSlots() map[string]string {
	nodeSlots := make(map[string]string)
	addrs := conf.Content.Instances
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
	return nodeSlots
}

func clusterSlots(c types.CommandContext) {
	conn := c.Conn
	addrs := conf.Content.Instances
	nodeSlots := getNodeSlots()
	conn.WriteArray(len(addrs))
	for _, addr := range addrs {
		conn.WriteArray(3)
		addrSlice := strings.Split(addr, ":")
		slotRange := strings.Split(nodeSlots[addr], "-")
		uuid := utils.MD5With40(addr)
		startSlot, _ := strconv.ParseInt(slotRange[0], 10, 64)
		endSlot, _ := strconv.ParseInt(slotRange[1], 10, 64)
		port, _ := strconv.ParseInt(addrSlice[1], 10, 64)
		conn.WriteInt64(startSlot)
		conn.WriteInt64(endSlot)
		conn.WriteArray(4)
		conn.WriteBulkString(addrSlice[0])
		conn.WriteInt64(port)
		conn.WriteBulkString(uuid)
		conn.WriteArray(0)
	}
}

func clusterTest(c types.CommandContext) {
	conn := c.Conn
	conn.WriteError("MOVED 5465 127.0.0.1:32681")
}
