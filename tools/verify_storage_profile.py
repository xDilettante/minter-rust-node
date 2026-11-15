"""Verify native full/archive/custom storage settings and actual mainnet data retention."""
import argparse
import io
import json
from pathlib import Path
import platform
import shutil
import subprocess
import time

from storage_footprint import closed_directory_footprint
from verify_mainnet_empty import read_varint, varint
from verify_native_sync import decode_trace, header
from verify_send_devnet import HIDE_WINDOW, logs, require


def verify(args):
    output = args.output
    output.mkdir(parents=True)
    db = output/'state'
    shutil.copytree(args.seed, db, ignore=lambda directory,names: [n for n in names if n.endswith('_key.json')])
    durations = {}
    def run(name, options, error=None):
        path = output/(name+'.log')
        before = time.monotonic()
        with path.open('xb') as log:
            result = subprocess.run([args.daemon,*options], stdout=log, stderr=subprocess.STDOUT,
                                    timeout=1800, creationflags=HIDE_WINDOW)
        durations[name] = round(time.monotonic()-before,3)
        if error:
            require(result.returncode != 0 and error in logs(path), f'{name}: {logs(path)[-3000:]}')
            return None
        require(result.returncode == 0, f'{name}: {logs(path)[-3000:]}')
        return decode_trace(path)[-1]
    def status(name):
        return run(name,['native-history','status','--db',db])
    initial = status('initial-status')
    head = initial['head']['height']
    require(head-initial['archiveFrom']+1 >= 100_000, initial)
    source = io.BytesIO(args.blocks.read_bytes())
    responses = {}
    while source.tell() < len(source.getbuffer()):
        raw = source.read(read_varint(source))
        responses[header(raw)[3]] = raw
    require(min(responses) == head, 'continuation differs from the seed')
    replay = ['native-sync','--db',db,'--blocks',args.blocks]
    invalid = output/'invalid-must-not-exist'
    run('profile-conflict',['native-sync','--db',invalid,'--blocks',args.blocks,'--through',str(head),
                           '--storage-profile','full','--keep-native-blocks','4'],'cannot be used with')
    run('invalid-body-count',['native-sync','--db',invalid,'--blocks',args.blocks,'--through',str(head),
                             '--keep-native-blocks','1'],'retain zero or at least two')
    require(not invalid.exists(),'invalid CLI settings created a database')
    sizes = {'initial':closed_directory_footprint(db)}
    run('full-profile',[*replay,'--through',str(head),'--storage-profile','full'])
    full = status('full-status')
    require(full['storageProfile']=='full' and full['retention']=={
        'keepIavlVersions':120,'keepEventBlocks':100000,'keepNativeBlocks':100000}, full)
    require(full['head']==initial['head'] and full['archiveFrom']==head-99999
            and full['eventsFrom']==head-99999 and full['eventsThrough']==head,full)
    run('full-reopened',[*replay,'--through',str(head)])
    require(status('full-reopened-status')['retention']==full['retention'],'profile did not persist')
    run('full-compaction',['native-history','maintain','--db',db,'--compact'])
    sizes['fullCompacted'] = closed_directory_footprint(db)
    run('archive-profile',[*replay,'--through',str(head+2),'--storage-profile','archive','--trace'])
    archive = status('archive-status')
    require(archive['storageProfile']=='archive' and archive['retention']=={
        'keepIavlVersions':0,'keepEventBlocks':0,'keepNativeBlocks':0},archive)
    require(archive['archiveFrom']==full['archiveFrom'] and archive['eventsFrom']==full['eventsFrom']
            and archive['head']=={'height':head+2,'appHash':header(responses[head+3])[11].hex()},archive)
    run('custom-profile',[*replay,'--through',str(head+2),'--keep-iavl-versions','2',
                          '--keep-event-blocks','2','--keep-native-blocks','4'])
    custom = status('custom-status')
    require(custom['storageProfile']=='custom' and custom['retention']=={
        'keepIavlVersions':2,'keepEventBlocks':2,'keepNativeBlocks':4},custom)
    require(custom['head']==archive['head'] and custom['archiveFrom']==head-1
            and custom['eventsFrom']==custom['iavlFrom']==head+1
            and custom['eventsThrough']==custom['iavlThrough']==head+2,custom)
    sizes['customBeforeCompaction'] = closed_directory_footprint(db)
    compacted = run('custom-compaction',['native-history','maintain','--db',db,'--compact'])
    require(compacted['compacted'],compacted)
    sizes['customCompacted'] = closed_directory_footprint(db)
    require(sizes['customCompacted']['allocatedBytes'] < sizes['initial']['allocatedBytes'],sizes)
    anchors = output/'anchors.pb'
    anchors.write_bytes(b''.join(varint(len(responses[h]))+responses[h] for h in range(head+2,head+5)))
    snapshot = output/'snapshot'
    exported = run('export',['native-snapshot','--db',db,'--output',snapshot,'--anchor-blocks',anchors,
                            '--with-history-context'])
    with (output/'go-reference.json').open('xb') as log:
        subprocess.run([args.go_verifier,snapshot/'history.pb',snapshot/'blocks.pb',snapshot/'state.pb'],
                       stdout=log,stderr=subprocess.STDOUT,timeout=180,check=True,creationflags=HIDE_WINDOW)
    go = json.loads((output/'go-reference.json').read_bytes())
    require(go['firstExpiredBoundaryVerified'] and go['from']==custom['contextFrom'],go)
    reopened = status('custom-reopened-status')
    require(reopened==custom,'compaction/export changed application or retention state')
    return {'platform':platform.platform(),'initial':initial,'full':full,'archive':archive,'custom':custom,
            'go':go,'snapshot':exported,'sizes':sizes,'durationSeconds':durations,
            'invalidSettingsLeaveNoDatabase':True,'profilePersists':True,'appStatePreservedByCleanup':True,
            'allThreeLimitsApplied':True,'allocatedFileBytesReduced':True}


if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','go_verifier','seed','blocks','output'):
        parser.add_argument(name,type=lambda p:Path(p).resolve())
    args=parser.parse_args()
    result=verify(args)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
