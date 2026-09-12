#!/usr/bin/env python3
"""Verify WordPress without printing cookies, passwords or response bodies."""
import base64
import http.cookiejar
import json
from pathlib import Path
import re
import urllib.error
import urllib.parse
import urllib.request


def verify(config):
    base = config['url'].rstrip('/') + '/'
    auth = 'Basic ' + base64.b64encode((config['visitor_user'] + ':' + config['visitor_password']).encode()).decode()
    def allowed(url):
        return urllib.parse.urlsplit(url)[:2] == urllib.parse.urlsplit(base)[:2] and urllib.parse.urlsplit(url).path.startswith(urllib.parse.urlsplit(base).path)
    class ScopedRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            if not allowed(newurl):
                raise RuntimeError('WordPress redirected outside its configured URL')
            return super().redirect_request(req, fp, code, msg, headers, newurl)
    cookies = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(ScopedRedirect, urllib.request.HTTPCookieProcessor(cookies))
    opener.addheaders = [("User-Agent", "GAP-WordPress/1.0")]
    def fetch(path, data=None):
        url = urllib.parse.urljoin(base, path)
        if not allowed(url): raise RuntimeError('Unexpected application URL')
        req = urllib.request.Request(url, data=data, headers={'Authorization':auth})
        with opener.open(req, timeout=120) as response:
            if response.status != 200: raise RuntimeError('Unexpected HTTP status')
            return response.read(4*1024*1024), response.geturl()
    # Without visitor credentials, the shared GAP URL must challenge.
    try:
        urllib.request.build_opener(ScopedRedirect).open(urllib.request.Request(base, headers={'User-Agent':'GAP-WordPress/1.0'}), timeout=120).close()
        raise RuntimeError('GAP visitor authentication is not enforced')
    except urllib.error.HTTPError as error:
        if error.code != 401: raise RuntimeError('Expected GAP Basic Auth challenge') from None
    home, _ = fetch('')
    sources = re.findall(rb'<script[^>]+src=[\'"]([^\'"]+)', home)
    if not sources: raise RuntimeError('No JavaScript asset found')
    fetch(sources[0].decode().replace('&amp;', '&'))
    rest, _ = fetch('wp-json/')
    if 'namespaces' not in json.loads(rest): raise RuntimeError('WordPress REST API unavailable')
    fetch('wp-login.php')
    form = urllib.parse.urlencode({'log':config['admin_user'], 'pwd':config['admin_password'],
        'wp-submit':'Log In', 'redirect_to':base+'wp-admin/', 'testcookie':'1'}).encode()
    _, final = fetch('wp-login.php', form)
    if '/wp-admin/' not in urllib.parse.urlsplit(final).path or not any(c.name.startswith('wordpress_logged_in_') for c in cookies):
        raise RuntimeError('WordPress administrator login failed')
    print('PASS: visitor authentication, home, JavaScript, REST API, administrator login')


if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state', type=Path, required=True)
    args = parser.parse_args()
    try: verify(json.loads(args.state.read_text()))
    except Exception as error: raise SystemExit('Verification failed: '+type(error).__name__) from None
