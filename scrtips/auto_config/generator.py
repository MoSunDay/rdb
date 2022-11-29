# Does not depend on third-party libraries
from uuid import uuid4
from pathlib import Path

def main():
    token = uuid4().hex
    template = '''
bind: {ip}:32681
store_path: /data/
backup_bind: {ip}:32682
backup_store_path: /data02/
raft_http_bind_address: {ip}:12681
raft_token: {token}
raft_bind_address: {ip}:22681
monitor_addr: {ip}:42681
    '''.strip()

    leader_conf = '''
  {ip}:22681:
    src: {ip}:32681
    target: {next_ip}:32682
'''[1:]

    output_path = Path("./output")
    if not Path.exists(output_path):
        Path.mkdir(output_path)
    with open('./ip_list', 'r') as f:
        ip_list = [ip.strip() for ip in f]
        
        for index, ip in enumerate(ip_list):
            ip = ip.strip()
            config = template.format(ip=ip, token=token)
            if index == 0:
                config += "\n\n# only leader\nbackup_target_map:\n"
                for i, _ip in enumerate(ip_list):
                    config += leader_conf.format(ip=_ip, next_ip=ip_list[(i+1) % len(ip_list)])

            with open(f'{output_path}/conf_{ip}', "w") as f:
                f.write(config)
            supervisor_conf = f'''
[supervisord]

[supervisorctl]
serverurl = unix:///var/run/supervisor.sock

[unix_http_server]
file = /var/run/supervisor.sock

[rpcinterface:supervisor]
supervisor.rpcinterface_factory = supervisor.rpcinterface:make_main_rpcinterface

[inet_http_server]
port=127.0.0.1:9001

[program:rdb]
directory=/app
{"environment=RAFT_BOOTSTRAP=true" if index == 0 else f"environment=RAFT_JOIN_ADDR={ip_list[0]}:12681"}
command=/app/rdb -config ./conf.yaml
stderr_logfile=/app/rdb_stderr.log
stdout_logfile=/app/rdb_stdout.log
stdout_logfile_maxbytes=10MB
stdout_logfile_backups=10
stderr_logfile_maxbytes=10MB
stderr_logfile_backups=10    
'''
            with open(f'{output_path}/supervisor_{ip}', "w") as f:
                f.write(supervisor_conf)

        
if __name__ == '__main__':
    main()