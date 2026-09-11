#!/usr/bin/env python3
"""Operator-only: allow the reserved DNAT range through Docker's user firewall."""
import argparse
import ipaddress
import os
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--public-ip', required=True)
    parser.add_argument('--first-port', type=int, default=24000)
    parser.add_argument('--last-port', type=int, default=24099)
    args = parser.parse_args()
    address = ipaddress.IPv4Address(args.public_ip)
    if os.geteuid() != 0 or not 1024 <= args.first_port <= args.last_port <= 65535:
        parser.error('requires root and a valid high port range')
    for protocol in ('tcp', 'udp'):
        rule = ['-p', protocol, '-m', 'conntrack', '--ctstate', 'DNAT',
                '--ctorigdst', str(address), '--ctorigdstport', f'{args.first_port}:{args.last_port}',
                '-m', 'comment', '--comment', 'gap-microvm-public-ports', '-j', 'ACCEPT']
        if subprocess.run(['iptables', '-w', '-C', 'DOCKER-USER', *rule], capture_output=True).returncode:
            subprocess.run(['iptables', '-w', '-I', 'DOCKER-USER', '1', *rule], check=True)
    print('GAP reserved TCP/UDP DNAT range allowed')


if __name__ == '__main__':
    main()
