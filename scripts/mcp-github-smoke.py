#!/usr/bin/env python3
"""Exercise the real stdio MCP lifecycle against explicitly named public repositories.

Creates a new project (never overwrites), restarts the MCP server, plans and runs it
from disk twice. Clone destinations must be supplied explicitly. This makes real
GitHub requests and clones/fetches; it is not part of the offline test suite.
"""
import argparse
import json
import pathlib
import queue
import subprocess
import threading


class Client:
    def __init__(self, binary, output_dir, transcript):
        self.process = subprocess.Popen(
            [str(binary), '--output-dir', str(output_dir), '--allow-apply'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
        )
        self.incoming = queue.Queue()
        self.transcript = transcript
        self.seq = 0
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()
        self.request('initialize', {
            'protocolVersion': '2025-11-25', 'capabilities': {},
            'clientInfo': {'name': 'ppduster-mcp-smoke', 'version': '1.0'},
        })
        self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})

    def _read(self):
        for line in self.process.stdout:
            self.incoming.put(json.loads(line))
        self.incoming.put(None)

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + '\n')
        self.process.stdin.flush()

    def request(self, method, params):
        self.seq += 1
        request = {'jsonrpc': '2.0', 'id': self.seq, 'method': method, 'params': params}
        self.send(request)
        while True:
            response = self.incoming.get(timeout=180)
            if response is None:
                raise RuntimeError('MCP closed stdout before responding')
            if response.get('id') == self.seq:
                break
        self.transcript.append({'request': request, 'response': response})
        if 'error' in response:
            raise RuntimeError(response['error'])
        return response['result']

    def call(self, name, arguments):
        result = self.request('tools/call', {'name': name, 'arguments': arguments})
        data = result.get('structuredContent')
        if data is None:
            texts = [item['text'] for item in result.get('content', []) if item['type'] == 'text']
            data = json.loads('\n'.join(texts))
        print(json.dumps({'tool': name, 'success': not result.get('isError', False),
                          'error': data.get('error')}, ensure_ascii=False), flush=True)
        if result.get('isError'):
            raise RuntimeError(json.dumps(data, ensure_ascii=False))
        return data

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.reader.join(timeout=2)
        self.process.stdout.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--output-dir', type=pathlib.Path, required=True)
    parser.add_argument('--repository', action='append', required=True)
    parser.add_argument('--destination', type=pathlib.Path, required=True)
    parser.add_argument('--output-path', required=True)
    parser.add_argument('--transcript', type=pathlib.Path, required=True)
    args = parser.parse_args()
    transcript = []
    client = None
    try:
        client = Client(args.binary.resolve(), args.output_dir.resolve(), transcript)
        tools = client.request('tools/list', {})
        print(json.dumps({'tools': [tool['name'] for tool in tools['tools']]}), flush=True)
        repositories = client.call('list_github_repositories', {})['repositories']
        by_name = {repo['full_name']: repo for repo in repositories}
        selected = [by_name[name] for name in args.repository]
        assert all(repo['selectable'] for repo in selected), 'Select only public, active repositories'
        created = client.call('create_github_scheme', {
            'repository_ids': [repo['id'] for repo in selected],
            'destination_root': str(args.destination.resolve()), 'output_path': args.output_path,
        })
        client.close()
        # No GitHub preview is loaded in this second process.
        client = Client(args.binary.resolve(), args.output_dir.resolve(), transcript)
        client.call('read_scheme', {'path': args.output_path})
        request = {'path': args.output_path, 'scenario_id': created['scenario_id']}
        plan = client.call('plan_scheme', request)
        first = client.call('run_scheme', request)
        before = {}
        for name in args.repository:
            checkout = args.destination.resolve() / name
            before[name] = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip()
            marker = checkout / '.ppduster-mcp-local-work'
            with marker.open('x') as handle:
                handle.write('local uncommitted work must survive a repeated run\n')
        second = client.call('run_scheme', request)
        for name, head in before.items():
            checkout = args.destination.resolve() / name
            assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == head
            assert (checkout / '.ppduster-mcp-local-work').read_text() == 'local uncommitted work must survive a repeated run\n'
        for label, result in [('plan', plan), ('first_run', first), ('second_run', second)]:
            print(json.dumps({label: [{'step': step['step_id'], 'status': step['status'], 'summary': step['summary']}
                                     for step in result['report']['steps']]}, ensure_ascii=False), flush=True)
        print(json.dumps({'verified': True, 'saved_project': created['path'], 'destination': str(args.destination)}, ensure_ascii=False), flush=True)
    finally:
        if client is not None:
            client.close() if client.process.poll() is None else None
        args.transcript.write_text(json.dumps(transcript, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
