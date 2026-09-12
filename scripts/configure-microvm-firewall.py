#!/usr/bin/env python3
"""Allow the configured VM public port pool through Elestio's Docker firewall."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import shlex

BEGIN = '# BEGIN GAP MICROVM PUBLIC PORTS'
END = '# END GAP MICROVM PUBLIC PORTS'
MARKER = '# Block all new external connections to Docker ports not explicitly allowed above'


def configure(config, script, apply=False):
    network=json.loads(Path(config).read_text())['hypervisor']['public_network']
    first,last=network['first_port'],network['last_port']
    if type(first) is not int or type(last) is not int or not 1024<=first<=last<=65535 or last-first>=1000:
        raise ValueError('Invalid managed public port pool')
    if any(first<=port<=last for port in (8080,8090,8091,8092,8093,8094,8123,9000)):
        raise ValueError('Public pool overlaps internal control ports')
    path=Path(script)
    if path.is_symlink():raise ValueError('Refusing a symlink firewall script')
    source=path.read_text()
    if source.count(MARKER)!=1:raise ValueError('Unknown firewall layout; inspect before applying')
    rules=[]
    for protocol in ('tcp','udp'):
        rules.append(['-p',protocol,'-m',protocol,'--dport',f'{first}:{last}','-m','conntrack','--ctorigdstport',f'{first}:{last}','-m','comment','--comment','gap-microvm-public','-j','ACCEPT'])
    block=BEGIN+'\n'+''.join('iptables -w 5 -C DOCKER-USER '+' '.join(r)+' 2>/dev/null || iptables -w 5 -I DOCKER-USER '+' '.join(r)+'\n' for r in rules)+END+'\n\n'
    stripped=re.sub(re.escape(BEGIN)+r'.*?'+re.escape(END)+r'\n*','',source,flags=re.S)
    updated=stripped.replace(MARKER,block+MARKER)
    print(f'Managed VM public pool: TCP/UDP {first}-{last}; worker control ports remain excluded')
    if not apply:return
    backup=path.with_name(path.name+'.gap-before')
    if not backup.exists():backup.write_bytes(path.read_bytes());backup.chmod(0o600)
    if updated!=source:path.write_text(updated)
    # Apply only these two rules. Do not rerun the provider script, flush rules,
    # restart Docker, or interrupt existing sessions.
    existing_rules=subprocess.check_output(['iptables','-w','5','-S','DOCKER-USER'],text=True)
    for line in existing_rules.splitlines():
        parts=shlex.split(line)
        if '--comment' in parts and parts[parts.index('--comment')+1]=='gap-microvm-public' and parts[2:] not in rules:
            subprocess.run(['iptables','-w','5','-D','DOCKER-USER',*parts[2:]],check=True)
    for rule in rules:
        exists=subprocess.run(['iptables','-w','5','-C','DOCKER-USER',*rule],capture_output=True).returncode==0
        if not exists:subprocess.run(['iptables','-w','5','-I','DOCKER-USER',*rule],check=True)
    print('Rules applied and persisted in '+str(path))


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--config',default='data/gap-compose/config/runner.json')
    p.add_argument('--script',default='/opt/docker-firewall-rules.sh')
    p.add_argument('--apply',action='store_true')
    a=p.parse_args();configure(a.config,a.script,a.apply)
