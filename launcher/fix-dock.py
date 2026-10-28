#!/usr/bin/python3 -I
# Points Dock items at Attesor launchers instead of the arm64 originals inside them.
#
# Keeping a running guest in the Dock records the bundle the guest runs from,
# X.app/Contents/Resources/Original/X.app, which shows the "prohibited" badge and cannot be opened.
# Rewrite such items (pinned and recent) to the launcher X.app, drop their cached bookmark (the Dock
# prefers it over the URL), and restart the Dock if anything changed. Items of a guest that is still
# running are left alone: the Dock writes the original's path back for as long as it runs.
import plistlib
import re
import subprocess
import sys
import urllib.parse

INNER = re.compile(r'^(file://.+?\.app)/Contents/Resources/Original/[^/]+\.app/?$')


def wrapped(url):
    path = urllib.parse.unquote(urllib.parse.urlparse(url).path)
    try:
        with open(path.rstrip('/') + '/Contents/Info.plist', 'rb') as f:
            return plistlib.load(f).get('AttesorWrapped') is True
    except OSError:
        return False


def running(inner_url):
    path = urllib.parse.unquote(urllib.parse.urlparse(inner_url).path).rstrip('/') + '/'
    return subprocess.run(['pgrep', '-f', path], capture_output=True).returncode == 0


def main():
    raw = subprocess.run(['defaults', 'export', 'com.apple.dock', '-'], capture_output=True, check=True).stdout
    dock = plistlib.loads(raw)
    changed = []
    for section in ('persistent-apps', 'recent-apps'):
        for item in dock.get(section, []):
            tile = item.get('tile-data', {})
            fd = tile.get('file-data', {})
            m = INNER.match(fd.get('_CFURLString', ''))
            if not m or not wrapped(m.group(1) + '/') or running(fd['_CFURLString']):
                continue
            fd['_CFURLString'] = m.group(1) + '/'
            fd['_CFURLStringType'] = 15
            tile.pop('book', None)
            changed.append(f"{section}: {tile.get('file-label', '?')} -> {urllib.parse.unquote(fd['_CFURLString'])}")
    if not changed:
        return
    subprocess.run(['defaults', 'import', 'com.apple.dock', '-'], input=plistlib.dumps(dock, fmt=plistlib.FMT_BINARY), check=True)
    subprocess.run(['killall', 'Dock'], check=False)
    for c in changed:
        print('  dock:', c)


if __name__ == '__main__':
    sys.exit(main())
