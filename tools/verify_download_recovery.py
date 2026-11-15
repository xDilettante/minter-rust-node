"""Kill a required-history import, reuse its complete cache and clean it after readiness."""
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
    output=args.output
    output.mkdir(parents=True)
    manifest=json.loads((args.checkpoint/'manifest.json').read_bytes())
    required=manifest['historyContext']
    head=manifest['height']
    cache_name='history-'+required['sha256']+'.pb'
    metadata=output/'metadata'
    metadata.mkdir()
    shutil.copyfile(args.checkpoint/'manifest.json',metadata/'manifest.json')
    trusted=hashlib.sha256((metadata/'manifest.json').read_bytes()).hexdigest()
    def prepare(name):
        db=output/name
        shutil.copytree(args.seed,db,ignore=lambda directory,names:[n for n in names if n.endswith('_key.json')])
        shutil.copyfile(args.checkpoint/'history.pb',db/cache_name)
        (db/'.history-download-interrupted').write_bytes(b'partial')
        (db/'context.pb').write_bytes(b'user-file')
        (db/'history-unknown.pb').write_bytes(b'user-file')
        folder=db/'.history-download-folder'
        folder.mkdir()
        (folder/'keep').write_bytes(b'user-file')
        return db
    def run(name,options):
        path=output/(name+'.log')
        with path.open('xb') as log:
            result=subprocess.run([args.daemon,*options],stdout=log,stderr=subprocess.STDOUT,
                                  timeout=600,creationflags=HIDE_WINDOW)
        require(result.returncode==0,f'{name}: {logs(path)[-3000:]}')
        return decode_trace(path)[-1]
    def status(name,db):
        return run(name,['native-history','status','--db',db])
    def untouched(db):
        require((db/'context.pb').read_bytes()==(db/'history-unknown.pb').read_bytes()==
                (db/'.history-download-folder/keep').read_bytes()==b'user-file','unrelated path changed')
    db=prepare('state')
    initial=status('initial',db)
    require(not initial['requiredHistoryReady'] and initial['contextFrom']==head,initial)
    require(not (db/'.history-download-interrupted').exists() and (db/cache_name).is_file(),'startup cache cleanup differs')
    untouched(db)
    path=output/'interrupted-import.log'
    process=start([args.daemon,'native-history','import','--db',db,'--input',db/cache_name],path)
    try:
        wait_for(lambda:'Imported 1000 new history records;' in logs(path),'durable context prefix',
                 [(process,path)],timeout=180)
        require(process.poll() is None,'import finished before the forced stop')
        process.kill()
        code=process.wait(timeout=20)
        require(code!=0,'import was not forcibly stopped')
    finally:
        stop(process)
    interrupted=status('after-stop',db)
    require(interrupted['head']==initial['head'] and not interrupted['requiredHistoryReady']
            and required['from']<interrupted['contextFrom']<=head-1000,interrupted)
    require(hashlib.sha256((db/cache_name).read_bytes()).hexdigest()==required['sha256'],'required cache was lost')
    # Only the original manifest is available; there is no source history file,
    # snapshot or peer in this restore command. It must use the retained cache.
    run('cache-resume',['native-sync','--db',db,'--checkpoint',metadata,'--checkpoint-sha256',trusted,
                        '--blocks',args.blocks,'--through',str(head),'--storage-profile','full'])
    require(not (db/cache_name).exists(),'ready context kept its completed cache')
    final=status('resumed-status',db)
    require(final['head']==initial['head'] and final['requiredHistoryReady']
            and final['contextFrom']==required['from'] and final['storageProfile']=='full',final)
    untouched(db)
    manual=prepare('manual-state')
    report=run('manual-import',['native-history','import','--db',manual,'--input',manual/cache_name])
    require(not (manual/cache_name).exists(),'manual import did not close and remove its completed cache')
    require(report['appended']==head-required['from'],report)
    manual_status=status('manual-status',manual)
    require(manual_status['head']==initial['head'] and manual_status['requiredHistoryReady'],manual_status)
    untouched(manual)
    return {'platform':platform.platform(),'initial':initial,'interrupted':interrupted,'final':final,
            'forcedExitCode':code,'manual':manual_status,'cacheBytes':required['bytes'],
            'interruptedFilesRemoved':True,'requiredCachePreservedAcrossKill':True,
            'manifestOnlyCacheResume':True,'completedCacheRemovedImmediately':True,
            'manualImportCacheRemovedImmediately':True,'unrelatedPathsPreserved':True}


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','seed','checkpoint','blocks','output'):
        parser.add_argument(name,type=lambda p:Path(p).resolve())
    args=parser.parse_args()
    result=verify(args)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
