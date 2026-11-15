"""Kill native-body cleanup mid-run and resume from a manifest-only checkpoint."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess

from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, logs, require, start, stop, wait_for


def verify(args):
    output = args.output
    output.mkdir(parents=True)
    db = output/'state'
    shutil.copytree(args.seed, db, ignore=lambda directory,names: [n for n in names if n.endswith('_key.json')])
    def run(name, options):
        path = output/(name+'.log')
        with path.open('xb') as log:
            result = subprocess.run([args.daemon,*options], stdout=log, stderr=subprocess.STDOUT,
                                    timeout=600, creationflags=HIDE_WINDOW)
        require(result.returncode == 0, f'{name}: {logs(path)[-3000:]}')
        return decode_trace(path)[-1]
    def status(name):
        return run(name,['native-history','status','--db',db])
    original = status('original')
    head, base = original['head']['height'], original['archiveFrom']
    count = 5000
    keep = head-(base+count)+1
    require(keep > 2 and original['keepNativeBlocks'] == 0, original)
    metadata = output/'metadata-only'
    metadata.mkdir()
    shutil.copyfile(args.checkpoint/'manifest.json',metadata/'manifest.json')
    trusted = hashlib.sha256((metadata/'manifest.json').read_bytes()).hexdigest()
    manifest = json.loads((metadata/'manifest.json').read_bytes())
    require(manifest['height'] == base, 'seed does not use the supplied original checkpoint')
    resume = ['native-sync','--db',db,'--checkpoint',metadata,'--checkpoint-sha256',trusted,
              '--blocks',args.blocks,'--through',str(head)]
    run('manifest-only-before',resume)
    path = output/'interrupted.log'
    process = start([args.daemon,'native-sync','--db',db,'--blocks',args.blocks,'--through',str(head),
                     '--keep-native-blocks',str(keep)],path)
    try:
        wait_for(lambda: 'cleanup retired 1000 full blocks;' in logs(path), 'first durable cleanup segment',
                 [(process,path)],timeout=240)
        require(process.poll() is None,'cleanup finished before the forced stop')
        process.kill()
        stopped = process.wait(timeout=20)
        require(stopped != 0,'cleanup did not receive a forced termination')
    finally:
        stop(process)
    interrupted = status('after-stop')
    require(interrupted['head'] == original['head']
            and base+1000 <= interrupted['archiveFrom'] < base+count
            and interrupted['contextFrom'] == interrupted['archiveFrom']
            and interrupted['keepNativeBlocks'] == keep, interrupted)
    # No old state.pb, blocks.pb, snapshot.zlib or history.pb is supplied.
    # The published index is relative to the new retained base, not bootstrap H.
    run('resumed-manifest-only',resume)
    final = status('after-resume')
    require(final['head'] == original['head'] and final['archiveFrom'] == base+count
            and final['contextFrom'] == base+count and final['archiveThrough'] == head, final)
    return {'platform':platform.platform(),'original':original,'interrupted':interrupted,'final':final,
            'forcedExitCode':stopped,'fullBlocksRetired':count,'manifestOnlyResume':True,
            'checkpointHeight':manifest['height'],'appStateUnchanged':True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','seed','checkpoint','blocks','output'):
        parser.add_argument(name,type=lambda p: Path(p).resolve())
    args = parser.parse_args()
    result = verify(args)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
