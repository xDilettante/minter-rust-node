"""Read the previous custom-profile root after compaction and reject the expired one."""
import argparse
import io
import json
from pathlib import Path
import subprocess

from verify_mainnet_empty import read_varint
from verify_native_sync import decode_trace, header
from verify_send_devnet import HIDE_WINDOW, logs, require


def verify(args):
    output = args.profile_case
    require((output/'result.json').is_file(),'complete the large profile case first')
    custom = json.loads((output/'result.json').read_bytes())['custom']
    head = custom['head']['height']
    require(custom['retention']['keepIavlVersions']==2 and custom['iavlFrom']==head-1, custom)
    source = io.BytesIO(args.blocks.read_bytes())
    roots = {}
    while source.tell() < len(source.getbuffer()):
        decoded = header(source.read(read_varint(source)))
        roots[decoded[3]-1] = decoded[11].hex()
    db = output/'state'
    proof_path = output/'custom-oldest-proof.json'
    with proof_path.open('xb') as log:
        subprocess.run([args.control,'iavl','prove','--db',db,'--key','00','--height',str(head-1)],
                       stdout=log,stderr=subprocess.STDOUT,timeout=60,check=True,creationflags=HIDE_WINDOW)
    proof = json.loads(proof_path.read_bytes())
    require(proof['height']==head-1 and proof['appHash']==roots[head-1],proof)
    with (output/'custom-oldest-verified.log').open('xb') as log:
        subprocess.run([args.control,'iavl','verify-proof','--proof',proof_path,'--height',str(head-1),
                        '--app-hash',roots[head-1],'--key','00'],stdout=log,stderr=subprocess.STDOUT,
                       timeout=60,check=True,creationflags=HIDE_WINDOW)
    path = output/'custom-expired-proof.log'
    with path.open('xb') as log:
        refused = subprocess.run([args.control,'iavl','prove','--db',db,'--key','00','--height',str(head-2)],
                                 stdout=log,stderr=subprocess.STDOUT,timeout=60,creationflags=HIDE_WINDOW)
    require(refused.returncode!=0 and 'outside the stored range' in logs(path),logs(path))
    return {'height':head-1,'appHash':roots[head-1],
            'oldestRootProofMatchesSignedHeaderAfterCompaction':True,'expiredVersionRefused':True,
            'verification':decode_trace(output/'custom-oldest-verified.log')[-1]}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('control','profile_case','blocks'):
        parser.add_argument(name,type=lambda p:Path(p).resolve())
    args = parser.parse_args()
    result = verify(args)
    with (args.profile_case/'custom-root-readback.json').open('x',encoding='utf-8') as output:
        output.write(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result))
