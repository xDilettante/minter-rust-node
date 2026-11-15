"""Replay the Go coin corpus, stop around Commit and verify persisted coin proofs."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import platform
import subprocess

from verify_candidate_commission import exchange,kill
from verify_mainnet_empty import app,fields
from verify_send_devnet import HIDE_WINDOW,require


def verify(args):
    args.output.mkdir(parents=True)
    digest=lambda path:hashlib.sha256(path.read_bytes()).hexdigest()
    inputs={name:digest(getattr(args,name)) for name in ('daemon','control','fixture')}
    scenarios=json.loads(args.fixture.read_bytes())
    results=[]
    for scenario in scenarios:
        name=scenario['name']
        require(Path(name).name==name and name not in ('.','..'),'invalid scenario name')
        case=args.output/name
        case.mkdir()
        database=case/'state'
        flags=['--transaction-chain-id',str(scenario.get('transactionChainId',1)),'--keep-iavl-versions','2']
        expected={'height':scenario['manifest']['height'],'appHash':scenario['manifest']['appHash']}
        if scenario.get('initialize'):
            with app(args.daemon,database,case/'initialize.log',*flags) as (engine,_):
                exchange(engine,scenario['initialize'],expected['height'])
                require(engine.info()==expected,'initialized head differs from Go')
        else:
            manifest,snapshot=case/'manifest.json',case/'snapshot.zlib'
            manifest.write_text(json.dumps(scenario['manifest']),encoding='utf-8')
            snapshot.write_bytes(base64.b64decode(scenario['snapshot'],validate=True))
            with (case/'import.log').open('xb') as output:
                subprocess.run([args.control,'checkpoint','import','--db',database,'--snapshot',snapshot,
                                '--manifest',manifest],check=True,timeout=120,stdout=output,
                               stderr=subprocess.STDOUT,creationflags=HIDE_WINDOW)
        stopped=[]
        for block in scenario['blocks']:
            height=block['height']
            # Go deliberately uses an isolated mempool for each recorded executor probe.
            for index,probe in enumerate(block.get('checks',[])):
                with app(args.daemon,database,case/f'check-{height}-{index}.log',*flags) as (engine,_):
                    exchange(engine,[probe],height)
                    require(engine.info()==expected,'CheckTx changed the committed head')
            with app(args.daemon,database,case/f'before-{height}.log',*flags) as (engine,process):
                require(engine.info()==expected,'head changed across opening')
                exchange(engine,block['exchanges'][:-1],height)
                require(engine.info()==expected,'uncommitted recreation changed the head')
                before=kill(process)
            with app(args.daemon,database,case/f'commit-{height}.log',*flags) as (engine,process):
                require(engine.info()==expected,'aborted recreation changed the head')
                exchange(engine,block['exchanges'],height)
                [(tag,_)]=fields(base64.b64decode(block['exchanges'][-1]['request'])).items()
                require(tag==11,'fixture does not finish with Commit')
                response=fields(base64.b64decode(block['exchanges'][-1]['response']))[tag+1]
                expected={'height':height,'appHash':fields(response)[2].hex()}
                require(engine.info()==expected,'committed head differs from Go')
                after=kill(process)
            stopped.append({'height':height,'beforeCommitExit':before,'afterCommitExit':after})
        proofs=[]
        with app(args.daemon,database,case/'final-reopened.log',*flags) as (engine,_):
            require(engine.info()==expected,'committed head did not survive opening')
            for row in scenario['blocks'][-1]['readings']:
                key=base64.b64decode(row['key'],validate=True)
                if not key.startswith(b'q'): continue
                query=engine.iavl_key(key,expected['height'])
                require(query.get(1,0)==0 and query[6]==key and query[9]==expected['height'],'coin query failed')
                require(query[7]==base64.b64decode(row['value'],validate=True),'physical coin state differs from Go')
                operation=fields(fields(query[8])[1])
                require(operation[2]==key,'proof key differs')
                proof={'height':expected['height'],'appHash':expected['appHash'],'key':key.hex(),
                       'value':query[7].hex(),'proof':operation[3].hex()}
                path=case/('proof-'+key.hex()+'.json')
                path.write_text(json.dumps(proof,indent=2)+'\n',encoding='utf-8')
                proofs.append((key,path))
        require(proofs,'fixture has no coin state to verify')
        with (case/'coin-proofs-verified.log').open('xb') as output:
            for key,path in proofs:
                subprocess.run([args.control,'iavl','verify-proof','--proof',path,'--height',str(expected['height']),
                                '--app-hash',expected['appHash'],'--key',key.hex()],check=True,timeout=30,
                               stdout=output,stderr=subprocess.STDOUT,creationflags=HIDE_WINDOW)
        result={'name':name,'blocks':len(scenario['blocks']),
                'deliveries':sum(len(block['exchanges'])-3 for block in scenario['blocks']),
                'isolatedCheckTxProbes':sum(len(block.get('checks',[])) for block in scenario['blocks']),
                'final':expected,'forcedStops':stopped,'verifiedCoinProofs':len(proofs)}
        results.append(result)
        print(name,'verified',flush=True)
    require(inputs=={name:digest(getattr(args,name)) for name in inputs},'runtime inputs changed')
    return {'platform':platform.platform(),'inputsSha256':inputs,'scenarios':results,
            'blocks':sum(row['blocks'] for row in results),'deliveries':sum(row['deliveries'] for row in results),
            'isolatedCheckTxProbes':sum(row['isolatedCheckTxProbes'] for row in results),
            'forcedStopsBeforeAndAfterCommit':2*sum(row['blocks'] for row in results),
            'verifiedCoinProofs':sum(row['verifiedCoinProofs'] for row in results),
            'allAbciResponsesAndRootsMatchGo':True,'uncommittedStateDiscarded':True,
            'committedStateSurvivesKill':True,'physicalModelsAndSymbolHistoryMatchGo':True}


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','control','fixture','output'):
        parser.add_argument(name,type=lambda p:Path(p).resolve())
    args=parser.parse_args()
    result=verify(args)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
