"""Check legacy mainnet restore, manifest-only recovery and explicit genesis trust."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess

from verify_mainnet_empty import fields
from verify_native_sync import decode_trace,header,response_stream
from verify_send_devnet import HIDE_WINDOW,logs,require


def verify(args):
    args.output.mkdir(parents=True)
    raw=(args.package/'manifest.json').read_bytes()
    manifest=json.loads(raw)
    digest=hashlib.sha256(raw).hexdigest()
    first=manifest['height']+1
    require(manifest['network']=='minter-mainnet-5' and 'genesisSha256' not in manifest
            and 'transactionChainId' not in manifest,'expected a legacy mainnet package')
    genesis_digest=hashlib.sha256(args.genesis.read_bytes()).hexdigest()
    require(genesis_digest=='906c2ab969389853247305b0930c92f5906dc1962cd3075832ca4d7ab0fe55a5',
            'expected the pinned official mainnet genesis')
    inputs=[args.daemon,args.genesis,args.blocks,*[args.package/name for name in
            ('manifest.json','snapshot.zlib','state.pb','blocks.pb','history.pb')]]
    hashes=lambda:{str(path):hashlib.sha256(path.read_bytes()).hexdigest() for path in inputs}
    before=hashes()
    (args.output/'inputs-before.json').write_text(json.dumps(before,indent=2)+'\n',encoding='utf-8')
    responses=response_stream(args.blocks,manifest['height'],first+4)
    database=args.output/'state'
    compared={}

    def run(name,options,error=None):
        path=args.output/(name+'.log')
        with path.open('xb') as output:
            result=subprocess.run([args.daemon,*options],stdout=output,stderr=subprocess.STDOUT,
                                  timeout=600,creationflags=HIDE_WINDOW)
        if error is not None:
            require(result.returncode!=0 and error in logs(path),f'{name}: {logs(path)[-2000:]}')
            return
        require(result.returncode==0,f'{name}: {logs(path)[-2000:]}')
        for row in decode_trace(path):
            if 'height' not in row: continue
            height=row['height']
            following=header(responses[height+1])
            require(row['appHash']==following[11].hex()
                    and row['blockHash']==fields(following[5])[1].hex()
                    and row['resultsHash']==following[12].hex(),f'signed mainnet roots differ at {height}')
            compared[height]=row

    # Supplying a matching manifest hash never makes foreign identity fields valid.
    for name,changes,error in (
        ('wrong-network-id',{'transactionChainId':2},'transaction chain ID differs'),
        ('wrong-genesis-digest',{'genesisSha256':'00'*32},'checkpoint genesis SHA-256 differs'),
    ):
        directory=args.output/name
        directory.mkdir()
        altered=(json.dumps({**manifest,**changes},indent=2)+'\n').encode()
        (directory/'manifest.json').write_bytes(altered)
        wrong_db=args.output/(name+'-state')
        run(name,['native-sync','--db',wrong_db,'--checkpoint',directory,
                  '--checkpoint-sha256',hashlib.sha256(altered).hexdigest(),
                  '--blocks',args.blocks,'--through',str(first)],error)
        require(not wrong_db.exists(),'foreign mainnet identity created a database')

    def restore(name,package,through,*,explicit=False):
        options=['native-sync','--db',database,'--checkpoint',package,'--checkpoint-sha256',digest,
                 '--blocks',args.blocks,'--through',str(through),'--storage-profile','full','--trace']
        if explicit: options+=['--genesis',args.genesis]
        run(name,options)

    restore('legacy-install',args.package,first+1)
    metadata=args.output/'manifest-only'
    metadata.mkdir()
    shutil.copyfile(args.package/'manifest.json',metadata/'manifest.json')
    restore('legacy-manifest-only-recovery',metadata,first+2)
    restore('legacy-explicit-official-genesis',args.package,first+3,explicit=True)
    require(sorted(compared)==list(range(first,first+4)),'incomplete mainnet replay')
    require(before==hashes(),'inputs changed during mainnet verification')
    run('final-status',['native-history','status','--db',database])
    final=json.loads(logs(args.output/'final-status.log'))
    require(final['head']=={'height':first+3,'appHash':compared[first+3]['appHash']}
            and final['requiredHistoryReady'],'mainnet restore did not finish')
    return {'platform':platform.platform(),'snapshotHeight':manifest['height'],
            'checkpointSha256':digest,'genesisSha256':genesis_digest,'comparedBlocks':len(compared),
            'checkedFrom':first,'checkedThrough':first+3,'final':final,
            'legacyManifestOnlyRecovery':True,'legacyNoneDigestWithExplicitGenesis':True,
            'invalidMainnetFieldsRejectedBeforeDatabase':True,'inputsStableAcrossRun':True,
            'inputsSha256':before}


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','package','blocks','genesis','output'):
        parser.add_argument(name,type=lambda p:Path(p).resolve())
    args=parser.parse_args()
    result=verify(args)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
