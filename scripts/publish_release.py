"""Publish dist/airstream.exe as a GitHub release (token from Git Credential Manager, never printed).
usage: python scripts/publish_release.py <version> <commit> "<title>" "<notes>"
"""
import hashlib, json, os, subprocess, sys, urllib.request

REPO = "tOnkowzl/airstream"
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
version, commit, title, notes = sys.argv[1:5]
cred = subprocess.run(["git", "credential", "fill"], input="protocol=https\nhost=github.com\n\n",
                      capture_output=True, text=True, creationflags=0x08000000).stdout
token = dict(l.split("=", 1) for l in cred.splitlines() if "=" in l)["password"]
H = {"Authorization": f"Bearer {token}", "Accept": "application/vnd.github+json", "User-Agent": "airstream-publish"}

def api(url, data=None, headers=None, method=None):
    req = urllib.request.Request(url, data=data, headers={**H, **(headers or {})}, method=method)
    with urllib.request.urlopen(req, timeout=300) as r:
        return json.load(r)

exe = os.path.join(ROOT, "dist", "airstream.exe")
blob = open(exe, "rb").read()
rel = api(f"https://api.github.com/repos/{REPO}/releases",
          json.dumps({"tag_name": f"v{version}", "target_commitish": commit, "name": title, "body": notes}).encode(),
          {"Content-Type": "application/json"})
up = rel["upload_url"].split("{")[0] + "?name=airstream.exe"
asset = api(up, blob, {"Content-Type": "application/octet-stream"})
url = asset["browser_download_url"]
with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "x"}), timeout=300) as r:
    public = r.read()
print(json.dumps({"version": version, "url": url, "sha256": hashlib.sha256(blob).hexdigest(),
                  "public_matches": hashlib.sha256(public).hexdigest() == hashlib.sha256(blob).hexdigest()}))
