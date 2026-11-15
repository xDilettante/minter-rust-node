"""Compare Go key-replacement cases over ABCI, including durable recovery."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
from types import SimpleNamespace
import subprocess
import socket

from verify_candidate_commission import exchange, kill, verify as replay
from verify_mainnet_empty import Abci, app, fields
from verify_send_devnet import HIDE_WINDOW, require


def verify(args):
    args.output.mkdir(parents=True)
    digest = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
    before = {name:digest(getattr(args,name)) for name in
              ('daemon','control','ordinary','frozen','payout')}
    results = {}
    for name in (() if args.payout_only else ('ordinary','frozen')):
        results[name] = replay(SimpleNamespace(daemon=args.daemon,control=args.control,
                               fixture=getattr(args,name),output=args.output/name))
        (args.output/name/'result.json').write_text(json.dumps(results[name],indent=2)+'\n',encoding='utf-8')
    case = json.loads(args.payout.read_bytes())
    output = args.output/'payout'
    output.mkdir()
    manifest = output/'manifest.json'
    snapshot = output/'snapshot.zlib'
    manifest.write_text(json.dumps(case['initial']['manifest']),encoding='utf-8')
    snapshot.write_bytes(base64.b64decode(case['initial']['snapshot'],validate=True))
    db = output/'state'
    with (output/'import.log').open('xb') as log:
        subprocess.run([args.control,'checkpoint','import','--db',db,'--snapshot',snapshot,
                        '--manifest',manifest],stdout=log,stderr=subprocess.STDOUT,
                       check=True,timeout=120,creationflags=HIDE_WINDOW)
    initial = {k:case['initial']['manifest'][k] for k in ('height','appHash')}
    require(initial['height']==719,'unexpected payout fixture height')
    require(case['control']['manifest']['appHash']==initial['appHash'],'different control state')
    with app(args.daemon,db,output/'failed-block.log') as (engine,process):
        peer = engine.connection.getpeername()
        exchange(engine,case['exchanges'],720)
        [(tag,payload)] = fields(base64.b64decode(case['endBlockRequest'],validate=True)).items()
        require(tag==10,'expected EndBlock request')
        try:
            engine.call(tag,payload)
        except RuntimeError as error:
            require(str(error)=='validator has no candidate record',f'unexpected rejection: {error}')
            rejection = str(error)
        else:
            raise RuntimeError('Rust accepted a block that Go cannot finish')
        # minterd closes the failed request's connection after ResponseException.
        with socket.create_connection(peer,timeout=10) as connection:
            with connection.makefile('rb') as reader:
                require(Abci(connection,reader).info()==initial,'failed EndBlock changed committed state')
        failed_exit = kill(process)
    control = case['control']['blocks'][0]
    with app(args.daemon,db,output/'normal-control-after-reopen.log') as (engine,process):
        require(engine.info()==initial,'failed block changed state after reopen')
        exchange(engine,control['exchanges'],720)
        commit = fields(base64.b64decode(control['exchanges'][-1]['response'],validate=True))[12]
        final = {'height':720,'appHash':fields(commit)[2].hex()}
        require(engine.info()==final,'normal payout differs from Go control')
        control_exit = kill(process)
    with app(args.daemon,db,output/'final-reopened.log') as (engine,_):
        require(engine.info()==final,'control payout did not survive reopening')
    results['payout'] = {'initial':initial,'final':final,'rustRejection':rejection,
                         'goPanic':case['panic'],'failedBlockExitCode':failed_exit,
                         'controlExitCode':control_exit,'failedEndBlockPreservesHead':True,
                         'normalControlMatchesGoAfterReopen':True,'forcedStops':2}
    require(before=={name:digest(getattr(args,name)) for name in before},'runtime inputs changed')
    return {'inputsSha256':before,'cases':results,
            'normalBlocks':sum(results.get(n,{}).get('blocks',0) for n in ('ordinary','frozen')),
            'normalTransactions':sum(results.get(n,{}).get('transactions',0) for n in ('ordinary','frozen')),
            'checkTxProbes':sum(results.get(n,{}).get('checkTxProbes',0) for n in ('ordinary','frozen')),
            'forcedStops':sum(results.get(n,{}).get('forcedStops',0) for n in ('ordinary','frozen'))+2,
            'normalCasesExecuted':not args.payout_only,
            'normalAbciResponsesAndRootsMatchGo':True if not args.payout_only else None,
            'failedBlockPreservesCommittedState':True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--payout-only',action='store_true')
    for name in ('daemon','control','ordinary','frozen','payout','output'):
        parser.add_argument(name,type=lambda value:Path(value).resolve())
    args = parser.parse_args()
    result = verify(args)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
