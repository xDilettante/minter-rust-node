"""Observe required-cache removal while the restored node is still serving P2P."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess

from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, logs, require, stop, wait_for


def verify(args):
    output = args.output
    output.mkdir(parents=True)
    db = output/'state'
    shutil.copytree(args.seed,db,ignore=lambda directory,names:[n for n in names if n.endswith('_key.json')])
    manifest = json.loads((args.checkpoint/'manifest.json').read_bytes())
    with (output/'initial-status.log').open('xb') as log:
        subprocess.run([args.daemon,'native-history','status','--db',db],stdout=log,
                       stderr=subprocess.STDOUT,timeout=45,check=True,creationflags=HIDE_WINDOW)
    initial = decode_trace(output/'initial-status.log')[-1]
    require(not initial['requiredHistoryReady'] and initial['head']['height']==manifest['height']
            and initial['contextFrom']>manifest['historyContext']['from'],initial)
    cache = db/('history-'+manifest['historyContext']['sha256']+'.pb')
    shutil.copyfile(args.checkpoint/'history.pb',cache)
    metadata = output/'metadata'
    metadata.mkdir()
    shutil.copyfile(args.checkpoint/'manifest.json',metadata/'manifest.json')
    digest = hashlib.sha256((metadata/'manifest.json').read_bytes()).hexdigest()
    path = output/'live-resume.log'
    command = [args.daemon,'native-sync','--db',db,'--checkpoint',metadata,
               '--checkpoint-sha256',digest,'--p2p-listen','127.0.0.1:0',
               '--storage-profile','full','--shutdown-on-stdin-close']
    with path.open('xb') as log:
        process = subprocess.Popen(command,stdin=subprocess.PIPE,stdout=log,stderr=subprocess.STDOUT,
                                   creationflags=HIDE_WINDOW)
    try:
        endpoint = wait_for(lambda:re.search(r'P2P listening on ([0-9a-f]+@127\.0\.0\.1:\d+)',logs(path)),
                            'restored live server',[(process,path)],timeout=600).group(1)
        require(not cache.exists() and process.poll() is None,'cache was not removed while serving')
        status_path = output/'live-peer.json'
        with status_path.open('xb') as log:
            subprocess.run([args.control,'peer','status','--peer',endpoint],stdout=log,
                           stderr=subprocess.STDOUT,timeout=45,check=True,creationflags=HIDE_WINDOW)
        peer = json.loads(status_path.read_bytes())
        require(peer['height']==manifest['height'] and process.poll() is None and not cache.exists(),peer)
        process.stdin.close()
        code = process.wait(timeout=30)
        require(code==0,logs(path)[-3000:])
    finally:
        if process.stdin and not process.stdin.closed:
            process.stdin.close()
        stop(process)
    with (output/'final-status.log').open('xb') as log:
        subprocess.run([args.daemon,'native-history','status','--db',db],stdout=log,
                       stderr=subprocess.STDOUT,timeout=45,check=True,creationflags=HIDE_WINDOW)
    final = decode_trace(output/'final-status.log')[-1]
    require(final['requiredHistoryReady'] and final['storageProfile']=='full'
            and final['head']==initial['head'],final)
    return {'platform':platform.platform(),'initial':initial,'peer':peer,'final':final,
            'cacheRemovedWhileProcessAlive':True,'p2pServedAfterCacheRemoval':True,
            'manifestOnlyLocalCacheResume':True,'gracefulExitCode':code}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','control','seed','checkpoint','output'):
        parser.add_argument(name,type=lambda p:Path(p).resolve())
    args = parser.parse_args()
    result = verify(args)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
