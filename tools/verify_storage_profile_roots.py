"""Exercise full IAVL=120 and archive growth, including historical ICS23 proofs."""
import argparse
import io
import json
from pathlib import Path
import platform
import shutil
import subprocess

from verify_mainnet_empty import read_varint
from verify_native_sync import decode_trace, header
from verify_send_devnet import HIDE_WINDOW, logs, require


def verify(args):
    output = args.output
    output.mkdir(parents=True)
    db = output/'state'
    shutil.copytree(args.seed, db, ignore=lambda directory,names: [n for n in names if n.endswith('_key.json')])
    def run(name, binary, options, refused=False):
        path = output/(name+'.json' if name.endswith('-proof') else name+'.log')
        with path.open('xb') as log:
            result = subprocess.run([binary,*options], stdout=log, stderr=subprocess.STDOUT,
                                    timeout=1800, creationflags=HIDE_WINDOW)
        if refused:
            require(result.returncode != 0 and 'outside the stored range' in logs(path), logs(path))
            return None
        require(result.returncode == 0, f'{name}: {logs(path)[-3000:]}')
        return decode_trace(path)[-1]
    def status(name):
        return run(name, args.daemon, ['native-history','status','--db',db])
    initial = status('initial')
    head = initial['head']['height']
    source = io.BytesIO(args.blocks.read_bytes())
    responses = {}
    while source.tell() < len(source.getbuffer()):
        raw = source.read(read_varint(source))
        responses[header(raw)[3]] = raw
    target = head+123
    replay = ['native-sync','--db',db,'--blocks',args.blocks]
    run('full', args.daemon, [*replay,'--through',str(target),'--storage-profile','full','--trace'])
    full = status('full-status')
    first = target-119
    require(full['storageProfile']=='full' and full['iavlFrom']==first and full['iavlThrough']==target, full)
    require(full['head']=={'height':target,'appHash':header(responses[target+1])[11].hex()}, full)
    def historical(name, height):
        proof = run(name+'-proof', args.control, ['iavl','prove','--db',db,'--key','00','--height',str(height)])
        root = header(responses[height+1])[11].hex()
        require(proof['height']==height and proof['appHash']==root, proof)
        return run(name+'-verified', args.control,
                   ['iavl','verify-proof','--proof',output/(name+'-proof.json'),
                    '--height',str(height),'--app-hash',root,'--key','00'])
    full_proof = historical('full-oldest', first)
    run('full-expired', args.control, ['iavl','prove','--db',db,'--key','00','--height',str(first-1)], refused=True)
    run('archive', args.daemon, [*replay,'--through',str(target+4),'--storage-profile','archive','--trace'])
    archive = status('archive-status')
    require(archive['storageProfile']=='archive' and archive['iavlFrom']==first
            and archive['iavlThrough']==target+4, archive)
    require(archive['head']=={'height':target+4,'appHash':header(responses[target+5])[11].hex()}, archive)
    archive_proof = historical('archive-oldest', first)
    run('archive-reopened', args.daemon, [*replay,'--through',str(target+4)])
    require(status('archive-reopened-status')==archive, 'archive changed across reopening')
    historical('archive-reopened-oldest', first)
    return {'platform':platform.platform(),'initial':initial,'full':full,'archive':archive,
            'blocksExecuted':127,'fullVersions':120,'archiveVersions':124,
            'fullOldestProof':full_proof,'archiveOldestProof':archive_proof,
            'historicalProofsMatchSignedHeaders':True,'expiredVersionRefused':True,
            'archiveGrowsBeyond120':True,'archivePersistsAcrossReopen':True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','control','seed','blocks','output'):
        parser.add_argument(name,type=lambda p:Path(p).resolve())
    args = parser.parse_args()
    result = verify(args)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
