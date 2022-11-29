#!/bin/bash

set -e

RDB_ROOT_PATH=/app
RDB_APP_PATH=${RDB_ROOT_PATH}/rdb

if [ ! -d ${RDB_ROOT_PATH} ]; then
    mkdir ${RDB_ROOT_PATH}
fi

sleep_time=1
rm -f ${RDB_APP_PATH}
while ! curl -s -o ${RDB_APP_PATH} 30.49.121.197/api/v2/faas/rdb/version/latest -H 'token: mutUVBfesuq6'; do
    sleep $sleep_time
    sleep_time=$(($sleep_time+1))
    echo "curl failed, sleep $sleep_time and re-try..."
done
chmod a+x ${RDB_APP_PATH}

$(which supervisorctl) -c ${RDB_ROOT_PATH}/supervisor.conf reload
sleep 5
$(which supervisorctl) -c ${RDB_ROOT_PATH}/supervisor.conf status