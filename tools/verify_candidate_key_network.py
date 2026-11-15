"""Exercise candidate key changes with native Rust consensus and Go Tendermint."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

from verify_native_sync import decode_trace
from verify_native_consensus import delivery_digest
from verify_send_devnet import HIDE_WINDOW,Node,allocate_ports,logs,require,stop,wait_for


def verify(args):
    args.output.mkdir(parents=True)
    fixture=args.output/'fixture'
    helper=[args.fixture_tool,'--chain-id','minter-rust-key-context-fixture']
    subprocess.run([*helper,'--initial-height','101','key-context',args.fixture,fixture],
                   check=True,timeout=30,creationflags=HIDE_WINDOW)
    genesis=json.loads((fixture/'genesis.json').read_bytes())
    public=json.loads((fixture/'fixture-public.json').read_bytes())
    first=int(genesis['initial_height'])
    ports=allocate_ports(7)
    go=Node(args.output/'go',args.tendermint,args.go_app,fixture,ports[:3],True)
    for item,folder in [('key','config'),('state','data')]:
        shutil.copyfile(fixture/f'second_validator_{item}.json',go.home/folder/f'priv_validator_{item}.json')
    database=args.output/'source-state'
    source=None
    peer=None
    traces=[]
    source_log=None
    checked={}
    observer_heights=set()

    def run(command,name,timeout=90,error=None):
        path=args.output/(name+'.log')
        with path.open('xb') as stream:
            result=subprocess.run(command,stdout=stream,stderr=subprocess.STDOUT,
                                  timeout=timeout,creationflags=HIDE_WINDOW)
        if error is None:
            require(result.returncode==0,f'{name} failed: {logs(path)[-2500:]}')
        else:
            require(result.returncode!=0 and error in logs(path),f'{name}: expected {error}: {logs(path)[-2500:]}')
        return path

    def write(name,value):
        path=args.output/(name+'.json')
        path.write_text(json.dumps(value,indent=2)+'\n',encoding='utf-8')
        return path

    def signer(key,go_state,name):
        journal=args.output/name
        run([args.daemon,'validator-signer','import','--key',fixture/key,'--go-state',fixture/go_state,
             '--journal',journal,'--chain-id',genesis['chain_id']],name+'-import')
        return journal

    original=signer('priv_validator_key.json','priv_validator_state.json','original-signer')
    replacement=signer('replacement_validator_key.json','replacement_validator_state.json','replacement-signer')

    def launch(name,*,initial=False,new_signer=False,serve=None):
        nonlocal source,peer,source_log
        source_log=args.output/('source-'+name+'.log')
        traces.append(source_log)
        flags=['--genesis',fixture/'genesis.json','--storage-profile','full'] if initial else []
        if serve: flags += ['--serve-snapshot',serve]
        with source_log.open('xb') as stream:
            source=subprocess.Popen([args.daemon,'native-sync','--db',database,'--consensus','--mempool',
                '--trace','--p2p-listen',f'127.0.0.1:{ports[3]}','--shutdown-on-stdin-close',
                '--validator-key',fixture/('replacement_validator_key.json' if new_signer else 'priv_validator_key.json'),
                '--validator-journal',replacement if new_signer else original,*flags],
                stdin=subprocess.PIPE,stdout=stream,stderr=subprocess.STDOUT,creationflags=HIDE_WINDOW)
        found=wait_for(lambda: next((s.split()[-1] for s in logs(source_log).splitlines()
                                    if s.startswith('Tendermint 0.34 P2P listening on')),None),
                       'native source listener',[(source,source_log)])
        require(peer in (None,found),'source peer identity changed')
        peer=found

    def close_source():
        nonlocal source
        require(source is not None and source.poll() is None,'source stopped before requested shutdown')
        source.stdin.close()
        require(source.wait(timeout=12)==0,logs(source_log)[-2000:])
        source=None

    def rows():
        return {r['height']:r for path in traces for r in decode_trace(path) if 'height' in r}

    def check(height):
        if height in checked: return checked[height]
        row=wait_for(lambda: rows().get(height),f'Rust height {height}',go.processes+[(source,source_log)])
        wait_for(lambda: go.height()>height,'Go successor',go.processes+[(source,source_log)])
        current=go.rpc('commit',height=height)['signed_header']
        following=go.rpc('commit',height=height+1)['signed_header']
        write('signed-commit-'+str(height),current)
        write('signed-commit-'+str(height+1),following)
        require(row['appHash']==following['header']['app_hash'].lower()
                and row['blockHash']==current['commit']['block_id']['hash'].lower()
                and row['resultsHash']==following['header']['last_results_hash'].lower(),f'roots differ at {height}')
        results=write('block-results-'+str(height),go.rpc('block_results',height=height))
        end=json.loads(subprocess.check_output([*helper,'end-block',results],timeout=20,creationflags=HIDE_WINDOW))
        require(row['endResponseSha256']==end['endResponseSha256'],'EndBlock differs from Go')
        checked[height]=row
        return row

    def check_observer(path):
        for row in decode_trace(path):
            if 'height' not in row: continue
            expected=check(row['height'])
            for field in ('appHash','blockHash','resultsHash','transactions','validatorUpdates',
                          'deliverResponsesSha256','endResponseSha256','validatorsHash','nextValidatorsHash'):
                require(row[field]==expected[field],f'observer {field} differs at {row["height"]}')
            observer_heights.add(row['height'])

    def transaction(nonce,kind,*values):
        result=json.loads(subprocess.check_output([args.transaction_tool,fixture/'genesis.json',str(nonce),kind,*map(str,values)],
                          timeout=20,creationflags=HIDE_WINDOW))
        require(hashlib.sha256(bytes.fromhex(result['tx'])).hexdigest()==result['tendermintHash'],'transaction hash mismatch')
        write('transaction-'+str(nonce),result)
        accepted=go.rpc('broadcast_tx_sync',tx='0x'+result['tx'])
        require(accepted.get('code',0)==0,f'Go CheckTx rejected fixture: {accepted}')
        receipt=wait_for(lambda:go.rpc('tx',hash='0x'+result['tendermintHash']),
                         f'committed transaction {nonce}',go.processes+[(source,source_log)])
        require(receipt['tx_result'].get('code',0)==0,f'Go DeliverTx rejected fixture: {receipt}')
        write('receipt-'+str(nonce),receipt)
        height=int(receipt['height'])
        row=check(height)
        require(row['transactions']==1 and row['deliverResponsesSha256']==delivery_digest(receipt['tx_result']),
                f'DeliverTx differs from Go for transaction {nonce}')
        return height

    def status(db,name):
        return json.loads(logs(run([args.daemon,'native-history','status','--db',db],name)))

    def metadata_copy(package,name,manifest):
        target=args.output/name
        target.mkdir()
        for file in ('state.pb','blocks.pb'):
            shutil.copyfile(package/file,target/file)
        data=(json.dumps(manifest,indent=2)+'\n').encode()
        (target/'manifest.json').write_bytes(data)
        return target,hashlib.sha256(data).hexdigest()

    def restore(db,package,digest,through,name,*,genesis_path=None,error=None):
        command=[args.daemon,'native-sync','--db',db,'--checkpoint',package,
                 '--checkpoint-sha256',digest,'--genesis',genesis_path or fixture/'genesis.json',
                 '--snapshot-peer',peer,'--peer',peer,'--through',str(through),'--storage-profile','full','--trace']
        return run(command,name,timeout=120,error=error)

    def advance(db,height,name,error=None):
        return run([args.daemon,'native-sync','--db',db,'--peer',peer,'--through',str(height),'--trace'],name,error=error)

    try:
        launch('initial',initial=True)
        go.launch(None,'initial',use_rust=False,engine_args=('--p2p.persistent_peers',peer))
        wait_for(lambda:go.height()>=first+5,'initial mixed blocks',go.processes+[(source,source_log)])
        check(first+3)
        block=write('infraction-block',go.rpc('block',height=first+1))
        validators=write('infraction-validators',go.rpc('validators',height=first+1))
        proof=json.loads(subprocess.check_output([*helper,'vote',block,validators],timeout=20,creationflags=HIDE_WINDOW))
        accepted=proof['nilEvidence']
        run([*helper,'relay',peer,write('accepted-proof',accepted)],'relay-proof',timeout=20)
        def included():
            for height in range(first+5,go.height()+1):
                if accepted in (go.rpc('block',height=height)['block']['evidence']['evidence'] or []): return height
            return None
        included_height=wait_for(included,'included local proof',go.processes+[(source,source_log)])
        check(included_height)
        rotation=transaction(1,'rotate','minority','minority replacement')
        check(rotation+2)
        close_source()
        frozen_status=status(database,'closed-source-before-snapshot')
        snapshot_height=frozen_status['head']['height']
        export_db=args.output/'snapshot-source'
        shutil.copytree(database,export_db)
        launch('after-snapshot-freeze')
        wait_for(lambda:go.height()>=snapshot_height+3,'signed snapshot anchors',go.processes+[(source,source_log)])
        anchors=args.output/'anchor.pb'
        run([args.control,'peer','block','--peer',peer,'--chain',genesis['chain_id'],'--height',str(snapshot_height),
             '--height',str(snapshot_height+1),'--height',str(snapshot_height+2),'--output',anchors],'capture-anchor')
        package=args.output/'package'
        run([args.daemon,'native-snapshot','--db',export_db,'--output',package,
             '--anchor-blocks',anchors,'--with-history-context'],'export-package')
        manifest=json.loads((package/'manifest.json').read_bytes())
        require(manifest['height']==snapshot_height and manifest.get('frozenReferences'),'snapshot lacks frozen references')
        require(manifest['genesisSha256']==hashlib.sha256((fixture/'genesis.json').read_bytes()).hexdigest()
                and manifest['transactionChainId']==1,'snapshot identity differs from the trusted genesis')
        require(manifest['appHash']==go.rpc('commit',height=snapshot_height+1)['signed_header']['header']['app_hash'].lower(),
                'snapshot root differs from Go')
        # A supported extension also exercises the real serving path above16KiB.
        manifest['fixtureNote']='x'*(20*1024)
        package_raw=(json.dumps(manifest,indent=2)+'\n').encode()
        (package/'manifest.json').write_bytes(package_raw)
        digest=hashlib.sha256(package_raw).hexdigest()
        require(len(package_raw)>16*1024,'fixture did not exercise the former manifest limit')
        metadata,metadata_digest=metadata_copy(package,'metadata',manifest)
        require(metadata_digest==digest,'metadata changed the trusted manifest')
        close_source()
        launch('serving-snapshot',serve=package)
        wait_for(lambda:go.height()>=snapshot_height+6,'source serving the snapshot',go.processes+[(source,source_log)])
        target=go.height()-2
        wrong_genesis=args.output/'different-genesis.json'
        wrong_genesis.write_bytes((fixture/'genesis.json').read_bytes()+b'\n')
        wrong_db=args.output/'wrong-genesis-state'
        restore(wrong_db,metadata,digest,target,'reject-wrong-genesis',genesis_path=wrong_genesis,
                error='checkpoint genesis SHA-256 differs')
        require(not wrong_db.exists(),'wrong genesis created a database')
        bad_manifest=json.loads(json.dumps(manifest))
        bad_manifest['frozenReferences'][0]['end']=10_000_000
        bad_meta,bad_digest=metadata_copy(package,'bad-context-metadata',bad_manifest)
        observer=args.output/'restored-state'
        restore(observer,bad_meta,bad_digest,target,'reject-bad-frozen-range',error='frozen reference exceeds fund record')
        # Recovery on the same directory proves the failed context never published a native head.
        restored_log=restore(observer,metadata,digest,target,'state-sync-with-context')
        restored=status(observer,'restored-status')
        require(restored['head']['height']==target,'State Sync did not reach the requested height')
        synced=decode_trace(restored_log)
        require(synced and synced[-1]['appHash']==go.rpc('commit',height=target+1)['signed_header']['header']['app_hash'].lower(),
                'restored observer differs from Go')
        check_observer(restored_log)
        without=json.loads(json.dumps(manifest))
        del without['frozenReferences']
        cold_meta,cold_digest=metadata_copy(package,'without-context-metadata',without)
        cold=args.output/'without-context-state'
        restore(cold,cold_meta,cold_digest,target,'state-sync-without-context')
        second_rotation=transaction(2,'rotate','minority replacement','minority replacement 2')
        due=included_height+518400
        lock_height=transaction(3,'lock',due)
        check(lock_height+2)
        check_observer(advance(observer,lock_height+1,'restored-after-linked-row-write'))
        positive=status(observer,'restored-final-status')
        require(positive['head']['appHash']==go.rpc('commit',height=lock_height+2)['signed_header']['header']['app_hash'].lower(),
                'transferred frozen reference did not preserve Go state')
        advance(cold,lock_height+1,'missing-context-apphash-failure',error='AppHash differs')
        rejected=status(cold,'missing-context-status')
        require(rejected['head']['height']==lock_height-1,'negative control published an invalid block')
        require(rejected['head']['appHash']==go.rpc('commit',height=lock_height)['signed_header']['header']['app_hash'].lower(),
                'negative control lost its last correct head')
        active_rotation=transaction(4,'rotate','majority','majority replacement')
        wait_for(lambda:rows().get(active_rotation+1) and go.height()>=active_rotation+1,
                 'last block signed by the previous key',go.processes+[(source,source_log)])
        old_address=public['majority']['address']
        new_address=public['majority replacement']['address']
        old_commit=go.rpc('commit',height=active_rotation+1)['signed_header']['commit']
        require({s['validator_address'] for s in old_commit['signatures'] if s['block_id_flag']==2}=={old_address},
                'previous key did not sign H+1')
        close_source()
        launch('replacement',new_signer=True)
        wait_for(lambda:go.height()>=active_rotation+5,'new signer blocks',go.processes+[(source,source_log)])
        check(active_rotation+1)
        check(active_rotation+2)
        check(active_rotation+4)
        new_commit=go.rpc('commit',height=active_rotation+2)['signed_header']['commit']
        require({s['validator_address'] for s in new_commit['signatures'] if s['block_id_flag']==2}=={new_address},
                'replacement key did not sign H+2')
        require({v['address'] for v in go.rpc('validators',height=active_rotation+1)['validators']}=={old_address},
                'validator set changed before H+2')
        require({v['address'] for v in go.rpc('validators',height=active_rotation+2)['validators']}=={new_address},
                'new validator set is missing at H+2')
        check_observer(advance(observer,active_rotation+3,'restored-after-signer-switch'))
        switched=status(observer,'restored-signer-status')
        require(switched['head']['appHash']==go.rpc('commit',height=active_rotation+4)['signed_header']['header']['app_hash'].lower(),
                'restored observer failed the signer switch')
        checked_through=min(max(rows()),go.height()-1)
        for height in range(first,checked_through+1): check(height)
        report={'first':first,'includedHeight':included_height,'offlineRotationHeight':rotation,
                'activeRotationHeight':active_rotation,'firstReplacementSignature':active_rotation+2,
                'oldSignerAddress':old_address,'newSignerAddress':new_address,
                'validatorSwitchAtHPlus2':True,'head':max(rows()),'mixedApplicationRootsMatchGo':True,
                'snapshotHeight':snapshot_height,'snapshotManifestBytes':len(package_raw),
                'checkpointManifestSha256':digest,'snapshotSha256':manifest['snapshotSha256'],
                'secondOfflineRotationHeight':second_rotation,'linkedRowWriteHeight':lock_height,
                'snapshotContextCovered':True,'stateSyncMatchedGoAfterWrite':True,
                'missingContextDetectedWithoutCommittingBadRoot':True,'badContextRejectedAndRecovered':True,
                'wrongGenesisRejectedBeforeDatabase':True,'largeManifestServed':True,
                'restoredObserverFollowedNewSigner':True,'checkedThrough':checked_through,
                'comparedBlocks':len(checked),'comparedObserverBlocks':len(observer_heights),
                'comparedDeliverTx':4}
        write('result',report)
        print(json.dumps(report))
    finally:
        stop(source)
        go.close()


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','control','tendermint','go_app','fixture_tool','transaction_tool','fixture','output'):
        parser.add_argument(name,type=lambda p:Path(p).resolve())
    verify(parser.parse_args())
