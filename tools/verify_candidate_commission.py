"""Replay Go commission fixtures over ABCI with forced stops before and after Commit."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import platform
import subprocess

from verify_mainnet_empty import app, fields
from verify_send_devnet import HIDE_WINDOW, logs, require


def exchange(engine, steps, height):
    for index, step in enumerate(steps):
        [(tag, request)] = fields(base64.b64decode(step['request'],validate=True)).items()
        expected = fields(base64.b64decode(step['response'],validate=True))[tag+1]
        actual = engine.call(tag,request)
        require(actual==expected,f'ABCI differs at {height}, call {index}, request tag {tag}')


def kill(process):
    process.kill()
    code = process.wait(timeout=20)
    require(code!=0,'process was not forcibly stopped')
    return code


def verify(args):
    output = args.output
    output.mkdir(parents=True)
    digest = lambda path:hashlib.sha256(path.read_bytes()).hexdigest()
    inputs = {name:digest(getattr(args,name)) for name in ('daemon','control','fixture')}
    scenarios = json.loads(args.fixture.read_bytes())
    results = []
    for scenario in scenarios:
        name = scenario['name']
        require(Path(name).name==name and name not in ('.','..'),'invalid scenario name')
        case = output/name
        case.mkdir()
        db = case/'state'
        network = scenario.get('transactionChainId',1)
        flags = ['--transaction-chain-id',str(network),'--keep-iavl-versions','2']
        initial = {'height':scenario['manifest']['height'],'appHash':scenario['manifest']['appHash']}
        if scenario.get('initialize'):
            with app(args.daemon,db,case/'initialize.log',*flags) as (engine,_):
                exchange(engine,scenario['initialize'],initial['height'])
                require(engine.info()==initial,'initialized head differs from Go')
        else:
            manifest, snapshot = case/'manifest.json',case/'snapshot.zlib'
            manifest.write_text(json.dumps(scenario['manifest']),encoding='utf-8')
            snapshot.write_bytes(base64.b64decode(scenario['snapshot'],validate=True))
            with (case/'import.log').open('xb') as log:
                subprocess.run([args.control,'checkpoint','import','--db',db,'--snapshot',snapshot,
                                '--manifest',manifest],stdout=log,stderr=subprocess.STDOUT,
                               timeout=120,check=True,creationflags=HIDE_WINDOW)
        expected = initial
        stopped = []
        for block in scenario['blocks']:
            height = block['height']
            with app(args.daemon,db,case/f'before-{height}.log',*flags) as (engine,process):
                require(engine.info()==expected,'head changed across reopening')
                exchange(engine,block.get('checks',[]),height)
                exchange(engine,block['exchanges'][:-1],height)
                require(engine.info()==expected,'uncommitted block changed the head')
                before = kill(process)
            with app(args.daemon,db,case/f'commit-{height}.log',*flags) as (engine,process):
                require(engine.info()==expected,'stopped uncommitted block changed the head')
                exchange(engine,block.get('checks',[]),height)
                exchange(engine,block['exchanges'],height)
                [(tag,_)] = fields(base64.b64decode(block['exchanges'][-1]['request'])).items()
                require(tag==11,'fixture does not end with Commit')
                commit = fields(base64.b64decode(block['exchanges'][-1]['response']))[tag+1]
                expected = {'height':height,'appHash':fields(commit)[2].hex()}
                require(engine.info()==expected,'committed head differs from Go')
                after = kill(process)
            stopped.append({'height':height,'beforeCommitExit':before,'afterCommitExit':after})
        with app(args.daemon,db,case/'final-reopened.log',*flags) as (engine,_):
            require(engine.info()==expected,'last committed head did not survive reopening')
            query = engine.iavl_key(b'c',expected['height'])
            require(query.get(1,0)==0 and query[6]==b'c' and query[9]==expected['height'],
                    'candidate model query failed')
            operation = fields(fields(query[8])[1])
            require(operation[2]==b'c','proof key differs')
        path = case/'candidate-model.json'
        model = {'height':expected['height'],'appHash':expected['appHash'],'key':'63',
                 'value':query[7].hex(),'proof':operation[3].hex()}
        path.write_text(json.dumps(model),encoding='utf-8')
        with (case/'candidate-model-verified.log').open('xb') as log:
            subprocess.run([args.control,'iavl','verify-proof','--proof',path,
                            '--height',str(expected['height']),'--app-hash',expected['appHash'],'--key','63'],
                           stdout=log,stderr=subprocess.STDOUT,timeout=60,check=True,creationflags=HIDE_WINDOW)
        go_model = next(row['value'] for row in scenario['blocks'][-1]['readings']
                        if base64.b64decode(row['key'])==b'c')
        require(model['value']==base64.b64decode(go_model).hex(),'persisted candidate model differs from Go')
        results.append({'name':name,'transactionChainId':network,'initial':initial,'final':expected,
                        'blocks':len(scenario['blocks']),
                        'transactions':sum(len(b['exchanges'])-3 for b in scenario['blocks']),
                        'checkTxProbes':sum(len(b.get('checks',[])) for b in scenario['blocks']),
                        'forcedStops':stopped,'candidateModelsMatchGo':True,'candidateProofMatchesGoRoot':True})
        print(name,'verified',flush=True)
    require(inputs=={name:digest(getattr(args,name)) for name in inputs},'runtime inputs changed')
    return {'platform':platform.platform(),'inputsSha256':inputs,'scenarios':results,
            'blocks':sum(row['blocks'] for row in results),
            'transactions':sum(row['transactions'] for row in results),
            'checkTxProbes':sum(row['checkTxProbes'] for row in results),
            'forcedStops':2*sum(row['blocks'] for row in results),
            'allAbciResponsesAndRootsMatchGo':True,'uncommittedStateDiscarded':True,
            'committedStateSurvivesKill':True,'candidateModelsMatchGo':True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','control','fixture','output'):
        parser.add_argument(name,type=lambda path:Path(path).resolve())
    args = parser.parse_args()
    result = verify(args)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
