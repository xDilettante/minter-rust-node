"""Check bounded full bodies, compact history and fresh State Sync on real mainnet."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess

from verify_mainnet_empty import read_varint, varint
from verify_native_peer import cli_run, success
from verify_native_sync import decode_trace, header
from verify_send_devnet import HIDE_WINDOW, allocate_ports, logs, require, start, stop, wait_for


def verify(args):
    output = args.output
    output.mkdir(parents=True)
    manifest = json.loads((args.checkpoint / 'manifest.json').read_bytes())
    anchor = manifest['height']
    tip = anchor + 18
    responses = {}
    source = io.BytesIO(args.blocks.read_bytes())
    while source.tell() < len(source.getbuffer()):
        raw = source.read(read_varint(source))
        responses[header(raw)[3]] = raw
    require(list(responses) == list(range(anchor, anchor+26)), 'continuation range differs')
    db = output / 'state'
    shutil.copytree(args.seed, db)

    def run(name, command, error=None, timeout=600):
        path = output / (name + '.log')
        with path.open('xb') as log:
            result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                                    timeout=timeout, creationflags=HIDE_WINDOW)
        if error:
            require(result.returncode != 0 and error in logs(path), f'{name}: {logs(path)[-3000:]}')
            return None
        require(result.returncode == 0, f'{name}: {logs(path)[-3000:]}')
        return decode_trace(path)[-1]

    def status(name, database=db):
        return run(name, [args.daemon, 'native-history', 'status', '--db', database])

    def expected(height):
        return header(responses[height+1])[11].hex()

    replay = [args.daemon, 'native-sync', '--db', db, '--blocks', args.blocks, '--trace']
    run('archive-replay', [*replay, '--through', str(tip), '--keep-native-blocks', '0'])
    before = status('before-retention')
    require(before['head'] == {'height':tip, 'appHash':expected(tip)}
            and before['archiveFrom'] == anchor, before)
    retention = json.loads(subprocess.check_output([args.cli, 'iavl', 'history', '--db', db],
                                                   timeout=30, creationflags=HIDE_WINDOW))
    server = None

    def launch(name, package, through=None):
        nonlocal server
        port = allocate_ports(1)[0]
        path = output / (name + '.log')
        command = [args.daemon, 'native-sync', '--db', db, '--p2p-listen', f'127.0.0.1:{port}',
                   '--serve-snapshot', package, '--keep-native-blocks', '4']
        if through:
            command += ['--blocks', args.blocks, '--through', str(through), '--trace']
        server = start(command, path)
        ready = wait_for(lambda: re.search(r'P2P listening on ([0-9a-f]{40})@', logs(path)),
                         'retained native server', [(server, path)], timeout=120)
        if through:
            wait_for(lambda: any(row.get('through') == through for row in decode_trace(path)),
                     'retained source replay', [(server, path)], timeout=120)
        return f'{ready.group(1)}@127.0.0.1:{port}'

    try:
        peer = launch('old-snapshot-server', args.checkpoint)
        observed = success(cli_run(args.cli, 'status', '--chain', manifest['network'], '--peer', peer))
        require(observed['base'] == tip-3 and observed['height'] == tip, observed)
        metadata = output / 'old-metadata'
        metadata.mkdir()
        for filename in ('manifest.json', 'state.pb', 'blocks.pb'):
            shutil.copyfile(args.checkpoint / filename, metadata / filename)
        trusted = hashlib.sha256((metadata / 'manifest.json').read_bytes()).hexdigest()
        run('stale-snapshot-refused', [args.daemon, 'native-sync', '--db', output/'stale-receiver',
             '--checkpoint', metadata, '--checkpoint-sha256', trusted, '--snapshot-peer', peer,
             '--timeout-secs', '1', '--blocks', args.blocks, '--through', str(anchor)],
            'no State Sync peer served the trusted snapshot')
        require('is not advertised' in logs(output/'old-snapshot-server.log'), 'no stale snapshot diagnostic')
    finally:
        stop(server)
    after = status('after-retention')
    require(after['head'] == before['head'] and after['archiveFrom'] == tip-3
            and after['archiveThrough'] == tip and after['keepNativeBlocks'] == 4
            and after['contextFrom'] > before['contextFrom'] and after['requiredHistoryReady'], after)
    retained = json.loads(subprocess.check_output([args.cli, 'iavl', 'history', '--db', db],
                                                 timeout=30, creationflags=HIDE_WINDOW))
    require(retained == retention, 'native cleanup changed IAVL retention')
    run('collect-memberships', [args.daemon, 'native-history', 'maintain', '--db', db])
    anchors = output / 'fresh-anchors.pb'
    anchors.write_bytes(b''.join(varint(len(responses[h]))+responses[h] for h in range(tip, tip+3)))
    package = output / 'fresh-snapshot'
    exported = run('fresh-export', [args.daemon, 'native-snapshot', '--db', db, '--output', package,
                                   '--anchor-blocks', anchors, '--with-history-context'])
    with (output/'go-reference.json').open('xb') as log:
        subprocess.run([args.go_verifier, package/'history.pb', package/'blocks.pb', package/'state.pb'],
                       stdout=log, stderr=subprocess.STDOUT, check=True, timeout=180, creationflags=HIDE_WINDOW)
    go = json.loads((output/'go-reference.json').read_bytes())
    require(go['firstExpiredBoundaryVerified'] and go['from'] == exported['historyContext']['from'], go)

    # Preparing an independent export does not displace the configured served snapshot.
    try:
        launch('old-snapshot-reopened', args.checkpoint)
    finally:
        stop(server)
    receiver = output / 'network-state'
    try:
        peer = launch('fresh-snapshot-server', package, tip+4)
        metadata = output / 'fresh-metadata'
        metadata.mkdir()
        for filename in ('manifest.json', 'state.pb', 'blocks.pb'):
            shutil.copyfile(package/filename, metadata/filename)
        trusted = hashlib.sha256((metadata/'manifest.json').read_bytes()).hexdigest()
        result = run('fresh-network-restore', [args.daemon, 'native-sync', '--db', receiver,
            '--checkpoint', metadata, '--checkpoint-sha256', trusted, '--snapshot-peer', peer,
            '--peer', peer, '--through', str(tip+2), '--keep-native-blocks', '4', '--trace'])
        require(result['through'] == tip+2 and result['appHash'] == expected(tip+2), result)
        network = status('network-status', receiver)
        require(network['head'] == {'height':tip+2, 'appHash':expected(tip+2)}
                and network['requiredHistoryReady'] and network['keepNativeBlocks'] == 4, network)
    finally:
        stop(server)
    configured = status('source-final-status')
    require(configured['archiveFrom'] == tip+1 and configured['archiveThrough'] == tip+4, configured)
    run('archive-again', [*replay, '--through', str(tip+5), '--keep-native-blocks', '0'])
    archive = status('archive-final-status')
    require(archive['archiveFrom'] == tip+1 and archive['archiveThrough'] == tip+5
            and archive['keepNativeBlocks'] == 0 and archive['head']['appHash'] == expected(tip+5), archive)
    return {'platform':platform.platform(), 'anchor':anchor, 'before':before, 'after':after,
            'go':go, 'network':network, 'archiveAfterPolicyChange':archive,
            'retentionUnchanged':True, 'staleSnapshotWithheld':True, 'oldSnapshotReopened':True,
            'freshStateSyncAndBlockSync':True, 'archivePolicyStopsDeletion':True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('daemon','cli','go_verifier','seed','checkpoint','blocks','output'):
        parser.add_argument(name, type=lambda p: Path(p).resolve())
    args = parser.parse_args()
    result = verify(args)
    (args.output/'result.json').write_text(json.dumps(result, indent=2)+'\n', encoding='utf-8')
    print(json.dumps(result))
