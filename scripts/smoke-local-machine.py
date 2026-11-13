#!/usr/bin/env python3
"""Opt-in real rootless Podman/SSH test; never installs enclo or uses its daemon.

Build examples/ssh-machine/Containerfile first. Only the uniquely named test
container is removed; the image is retained for reuse. Requires Python stdlib,
Podman and OpenSSH, with permission to create containers and bind sockets.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import ssl
import socket
import shutil
import tempfile
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', default='localhost/enclo-ssh-test:latest')
    parser.add_argument('--ssh-user', default='developer', help='existing SSH account in the test image')
    parser.add_argument('--binary', type=Path, default=Path('target/debug/enclo'))
    parser.add_argument('--systemd', action='store_true', help='test the opt-in systemd mode (requires a systemd image)')
    parser.add_argument('--systemd-mask', action='append', default=[], help='additional image-specific unit to mask in the test container')
    parser.add_argument('--secrets', action='store_true', help='also test the real host secrets/proxy service using synthetic credentials')
    parser.add_argument('--managed-home', action='store_true', help='test persistent home ownership and initialization')
    parser.add_argument('--container-init', action='store_true', help='test per-instance initialization after home setup; enable lingering in systemd mode')
    parser.add_argument('--nested-podman', action='store_true', help='build and run nested containers without pulls; implies --managed-home')
    parser.add_argument('--nvidia', action='store_true', help='expose NVIDIA CDI GPUs and verify nvidia-smi')
    parser.add_argument('--nested-proc-mount', choices=['private', 'host'], default='private', help='explicit nested proc mode; host exposes outer-container proc')
    parser.add_argument('--desktop-dbus', action='store_true', help='use an isolated session bus, never the real desktop bus')
    parser.add_argument('--fixed-ssh-port', action='store_true', help='verify one exported SSH config survives recreation')
    args = parser.parse_args()
    if args.nested_proc_mount == 'host' and not args.nested_podman:
        parser.error('--nested-proc-mount=host requires --nested-podman')
    if args.nested_podman or args.container_init:
        args.managed_home = True
    binary = args.binary.resolve(strict=True)
    processes = []
    report = {}
    with tempfile.TemporaryDirectory(prefix='ecs-') as temporary:
        root = Path(temporary)
        config = root / 'config'
        runtime = root / 'runtime'
        (config / 'containers').mkdir(parents=True)
        (config / 'connections').mkdir()
        volume = f'enclo-smoke-{root.name}'
        (config / 'containers/dev.toml').write_text(
            f'[container]\nimage={json.dumps(args.image)}\nvolumes=[{json.dumps(volume + ":/persistent")}]\n[ssh]\nuser={json.dumps(args.ssh_user)}\n')
        if args.fixed_ssh_port:
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0))
                fixed_port = probe.getsockname()[1]
            with (config / 'containers/dev.toml').open('a') as stream:
                stream.write(f'host_port={fixed_port}\n')
        if args.managed_home:
            with (config / 'containers/dev.toml').open('a') as stream:
                stream.write("\n[home]\nenabled=true\n[home.init]\ncommand='echo initialized >> init-count; test $(id -u) = $(stat -c %u .)'\n")
        if args.container_init:
            import shlex
            user = shlex.quote(args.ssh_user)
            hook = f'test "$(id -u)" = 0; home=$(getent passwd {user} | cut -d: -f6); test -f "$home/init-count"; echo initialized >> /persistent/container-init-count'
            if args.systemd:
                hook += f'; loginctl enable-linger {user}'
            with (config / 'containers/dev.toml').open('a') as stream:
                stream.write('\n[container.init]\nuser="root"\ncommand=' + json.dumps(hook) + '\n')
        if args.nested_podman:
            with (config / 'containers/dev.toml').open('a') as stream:
                stream.write(f'\n[nested_podman]\nenabled=true\nproc_mount="{args.nested_proc_mount}"\n')
        if args.nvidia:
            with (config / 'containers/dev.toml').open('a') as stream:
                stream.write('\n[gpu]\nnvidia=["all"]\n')
        if args.desktop_dbus:
            with (config / 'containers/dev.toml').open('a') as stream:
                stream.write('\n[desktop.dbus]\nenabled=true\n')
        masks = ['systemd-oomd.service', 'systemd-oomd.socket',
                 'systemd-resolved.service', 'systemd-resolved-monitor.socket',
                 'systemd-resolved-varlink.socket'] + args.systemd_mask
        if args.systemd:
            with (config / 'containers/dev.toml').open('a') as stream:
                stream.write('\n[systemd]\nenabled=true\nmask=' + json.dumps(masks) + '\n')
        scope = hashlib.sha256(str(config.resolve()).encode()).hexdigest()
        container_name = f'enclo-{scope[:12]}-dev'
        env = dict(os.environ, ENCLO_CONFIG_DIR=str(config), ENCLO_RUNTIME_DIR=str(runtime), ENCLO_STATE_DIR=str(root / "state"))
        # Avoid shell/mise shims and inherited config-root overrides.
        env['PATH'] = '/usr/bin:/bin'

        proxy_requests = []
        tls_server = None
        tls_thread = None
        if args.secrets:
            service = binary.parent / 'enclo-secrets'
            client = binary.parent / 'enclo-container-client'
            assert service.is_file() and client.is_file(), 'Build enclo-secrets and enclo-container-client first'
            subprocess.run([str(service), 'gen-ca', '--output-dir', str(root / 'certs')],
                           check=True, capture_output=True, timeout=30)
            (root / 'bin').mkdir()
            backend = root / 'bin/enclo-secret-store-smoke'
            backend.write_text("#!/usr/bin/python3\nimport sys,json\nfor line in sys.stdin:\n r=json.loads(line)\n print(json.dumps({'requestId':r['requestId'],'status':'ok','value':'synthetic-smoke-value'}),flush=True)\n")
            backend.chmod(0o700)
            env['PATH'] = str(root / 'bin') + ':' + env['PATH']
            # Trust only this fixture CA for the host proxy's upstream TLS check.
            env['SSL_CERT_FILE'] = str(root / 'certs/ca.crt')
            (config / 'secrets.toml').write_text(
                '[store]\n[[store.backends]]\nname="smoke"\n'
                '[[store.routes]]\npatterns=["*"]\nbackend="smoke"\n'
                '[store.confirm]\nenabled=false\n'
                '[secrets.access]\nmode="deny-all"\nallow=["SMOKE_TOKEN"]\n'
                '[proxy]\nenabled=true\n[proxy.tls]\n'
                f'ca_cert={json.dumps(str(root / "certs/ca.crt"))}\n'
                f'ca_key={json.dumps(str(root / "certs/ca.key"))}\n'
                '[[proxy.rules]]\ndomain="127.0.0.1"\n'
                '[proxy.rules.auth]\ntype="bearer"\nsecret="PROXY_ONLY"\n')

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                proxy_requests.append(dict((k.lower(), v) for k, v in self.headers.items()))
                body = b'enclo-smoke-tunnel-ok\n'
                self.send_response(200)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        (config / 'connections/work.toml').write_text(
            '[target]\ncontainer="dev"\n[[forwards]]\ndirection="remote"\n'
            f'spec="127.0.0.1:18080:127.0.0.1:{server.server_port}"\n')

        if args.secrets:
            with (config / 'connections/work.toml').open('a') as stream:
                stream.write('\n[services.secrets]\nconfig="../secrets.toml"\nremote_proxy_port=18081\n'
                             f'client_binary={json.dumps(str(client))}\n')
            # Local HTTPS upstream: exercises both MITM trust in the session and
            # normal certificate verification by the host proxy. No public API.
            subprocess.run(['openssl', 'req', '-new', '-newkey', 'rsa:2048', '-nodes',
                            '-keyout', str(root / 'upstream.key'), '-out', str(root / 'upstream.csr'),
                            '-subj', '/CN=127.0.0.1'], check=True, capture_output=True, timeout=30)
            (root / 'upstream.ext').write_text('subjectAltName=IP:127.0.0.1\n')
            subprocess.run(['openssl', 'x509', '-req', '-in', str(root / 'upstream.csr'),
                            '-CA', str(root / 'certs/ca.crt'), '-CAkey', str(root / 'certs/ca.key'),
                            '-CAcreateserial', '-out', str(root / 'upstream.crt'), '-days', '1',
                            '-extfile', str(root / 'upstream.ext')], check=True, capture_output=True, timeout=30)
            tls_server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(str(root / 'upstream.crt'), str(root / 'upstream.key'))
            tls_server.socket = context.wrap_socket(tls_server.socket, server_side=True)
            tls_thread = threading.Thread(target=tls_server.serve_forever, daemon=True)
            tls_thread.start()

        def cli(*command, check=True):
            result = subprocess.run([str(binary), *command], env=env,
                                    capture_output=True, text=True, timeout=180)
            if check and result.returncode:
                raise RuntimeError(f'{command}: {result.stderr}')
            return result

        def data(*command):
            return json.loads(cli(*command).stdout)

        def shell(*command, **kwargs):
            return cli('session', 'work', '--', *command, **kwargs)

        def wait(condition):
            deadline = time.monotonic() + 30
            while not condition():
                if time.monotonic() > deadline:
                    raise RuntimeError('smoke test readiness timed out')
                time.sleep(0.1)

        def start_services():
            log = open(root / f'services-{len(processes)}.log', 'w+')
            process = subprocess.Popen([str(binary), 'services', 'run', 'work'],
                                       env=env, stdout=log, stderr=log)
            processes.append(process)
            try:
                def ready():
                    if process.poll() is not None:
                        log.seek(0)
                        raise RuntimeError(f'services exited: {log.read()}')
                    return (runtime / 'connections/work/ready').exists()
                wait(ready)
            finally:
                log.close()
            return process

        def tunnel():
            response = shell('bash', '-c',
                "exec 3<>/dev/tcp/127.0.0.1/18080; "
                "printf 'GET / HTTP/1.0\\r\\nHost: localhost\\r\\n\\r\\n' >&3; cat <&3")
            assert 'enclo-smoke-tunnel-ok' in response.stdout, response

        def secrets_check():
            if not args.secrets:
                return
            allowed = json.loads(shell('enclo', 'get', 'SMOKE_TOKEN').stdout)
            assert allowed['SMOKE_TOKEN'] == 'synthetic-smoke-value'
            assert shell('enclo', 'get', 'PROXY_ONLY', check=False).returncode != 0
            for url in [f'http://127.0.0.1:{server.server_port}/',
                        f'https://127.0.0.1:{tls_server.server_port}/']:
                result = shell('enclo', 'proxy', '--', 'curl', '--silent', '--show-error',
                               '--fail', '--max-time', '10', '--noproxy', '', url)
                assert result.stdout == 'enclo-smoke-tunnel-ok\n', result
                assert proxy_requests[-1].get('authorization') == 'Bearer synthetic-smoke-value'
                assert 'proxy-authorization' not in proxy_requests[-1]
            report['secrets_http_https'] = 'passed'

        try:
            if args.desktop_dbus:
                bus = subprocess.Popen(['dbus-daemon', '--session', '--nofork', '--nopidfile', '--print-address=1', f'--address=unix:path={root}/fixture-bus'], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                processes.append(bus)
                bus_address = bus.stdout.readline().strip()
                assert bus_address.startswith('unix:path=')
                machine_config = config / 'containers/dev.toml'
                machine_config.write_text(machine_config.read_text().replace('[desktop.dbus]\n', '[desktop.dbus]\naddress='+json.dumps(bus_address)+'\n'))
            print(f'Starting isolated {"systemd" if args.systemd else "init/sshd"} machine…', flush=True)
            initial = data('container', 'start', 'dev')
            report['initial_endpoint'] = initial
            exported = root / 'exported-ssh-config'
            if args.fixed_ssh_port:
                assert initial['port'] == fixed_port
                exported.write_text(cli('container', 'export', 'dev').stdout)
                subprocess.run(['ssh', '-F', str(exported), 'enclo-dev', 'true'], check=True, timeout=15)
            assert initial['running'] and initial['port'] > 0
            assert len(data('container', 'list')) == 1
            assert not data('services', 'list')
            state = Path(initial['state_directory'])
            identity = (state / 'client_ed25519').read_bytes()
            known_hosts = state / f"instances/{initial['container_id']}/known_hosts"
            pinned_key = known_hosts.read_bytes()

            supervisor = start_services()
            assert data('services', 'status', 'work')['transport_ready']
            if args.secrets:
                exported_client = root / 'client-ssh-config'
                exported_client.write_text(cli('container', 'export', 'dev').stdout)
                ordinary = subprocess.run(['ssh', '-F', str(exported_client), 'enclo-dev', '"$HOME/.local/bin/enclo" get SMOKE_TOKEN'], capture_output=True, text=True, timeout=15)
                assert ordinary.returncode == 0, ordinary.stderr
                assert 'SMOKE_TOKEN' in ordinary.stdout
                report['plain_ssh_client'] = 'user-local executable discovers services without session environment'

            assert shell('id', '-u').stdout.strip() == str(os.getuid())
            if args.container_init:
                assert shell('cat', '/persistent/container-init-count').stdout == 'initialized\n'
                cli('container', 'init', 'dev')
                assert shell('cat', '/persistent/container-init-count').stdout == 'initialized\n'
                cli('container', 'init', 'dev', '--rerun')
                assert shell('cat', '/persistent/container-init-count').stdout == 'initialized\ninitialized\n'
                if args.systemd:
                    shell('test', '-f', f'/var/lib/systemd/linger/{args.ssh_user}')
                    assert shell('systemctl', 'is-active', f'user@{os.getuid()}.service').stdout.strip() == 'active'
            if args.nvidia:
                assert 'GPU' in shell('nvidia-smi', '-L').stdout
                report['nvidia'] = 'GPU discovered'
            if args.desktop_dbus:
                shell('dbus-send', '--session', '--type=method_call', '--print-reply', '--dest=org.freedesktop.DBus', '/', 'org.freedesktop.DBus.ListNames')
                denied = shell('dbus-send', '--session', '--type=method_call', '--print-reply', '--dest=org.freedesktop.DBus', '/', 'org.freedesktop.DBus.UpdateActivationEnvironment', 'dict:string:string:ENCLO_TEST,denied', check=False)
                assert denied.returncode != 0
                report['desktop_dbus'] = 'private bus connected; privileged method denied'
            if args.nested_podman:
                shell('sh', '-eu', '-c', 'mkdir -p "$HOME/nested-build"; printf "FROM scratch\\nCOPY marker /marker\\n" > "$HOME/nested-build/Containerfile"; printf nested-preserved > "$HOME/nested-build/marker"; podman --remote=false build --network=none --pull=never -t localhost/enclo-inner:smoke "$HOME/nested-build"')
                shell('podman', '--remote=false', 'create', '--name', 'inner-marker', 'localhost/enclo-inner:smoke', '/marker')
                shell('podman', '--remote=false', 'cp', 'inner-marker:/marker', '/tmp/inner-marker')
                assert shell('cat', '/tmp/inner-marker').stdout == 'nested-preserved'
                # A rootfs made from the outer image runs a real process without pulling an image.
                shell('python3', '-c', "import pathlib,shutil,subprocess; root=pathlib.Path('/tmp/inner-root'); files=['/bin/sh']+[p for p in subprocess.check_output(['ldd','/bin/sh'],text=True).split() if p.startswith('/')]; [( (root / p.lstrip('/')).parent.mkdir(parents=True,exist_ok=True), shutil.copyfile(p,root / p.lstrip('/')), (root / p.lstrip('/')).chmod(0o755)) for p in files]")
                assert shell('podman', '--remote=false', 'run', '--rm', '--network=none', '--rootfs', '/tmp/inner-root', '/bin/sh', '-c', 'printf nested-ran').stdout == 'nested-ran'
                report['nested_podman'] = 'built image and ran nested process without pulls'
            report['pid1'] = shell('ps', '-p', '1', '-o', 'comm=').stdout.strip()
            if args.systemd:
                assert report['pid1'] == 'systemd'
                systemd_state = shell('systemctl', 'is-system-running', check=False).stdout.strip()
                report['systemd_state'] = systemd_state
                report['failed_units'] = shell('systemctl', '--failed', '--no-legend', '--plain').stdout.strip()
                assert systemd_state == 'running', report
                for unit in masks:
                    assert shell('systemctl', 'is-enabled', unit, check=False).stdout.strip() == 'masked'
                shell('sudo', '-n', 'systemd-run', '--unit=enclo-smoke', '/usr/bin/sleep', '120')
                assert shell('systemctl', 'is-active', 'enclo-smoke').stdout.strip() == 'active'
                shell('sudo', '-n', 'systemctl', 'stop', 'enclo-smoke')
                assert shell('systemctl', 'is-active', 'enclo-smoke', check=False).returncode != 0
            else:
                assert report['pid1'] in ('podman-init', 'catatonit', 'tini', 'docker-init'), report
            assert shell('getent', 'hosts', 'fedoraproject.org').stdout.strip()
            report['outbound_dns'] = 'passed'
            assert shell('printf', '%s', "literal ' ; $(false)").stdout == "literal ' ; $(false)"
            tunnel()
            secrets_check()

            print('Checking host-key enforcement and independent session lifetime…', flush=True)
            try:
                known_hosts.write_text('')
                rejected = shell('true', check=False)
                assert rejected.returncode != 0 and 'Host key verification failed' in rejected.stderr, rejected
            finally:
                known_hosts.write_bytes(pinned_key)
            session = subprocess.Popen([str(binary), 'session', 'work', '--', 'sleep', '120'],
                                       env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            processes.append(session)
            wait(lambda: len(data('sessions', 'list')) == 1)
            refusal = cli('container', 'stop', 'dev', check=False)
            assert refusal.returncode != 0 and 'active sessions' in refusal.stderr
            cli('services', 'stop', 'work')
            supervisor.wait(timeout=10)
            assert session.poll() is None
            assert data('container', 'status', 'dev')['running']
            supervisor = start_services()
            tunnel()
            secrets_check()
            assert session.poll() is None
            cli('sessions', 'stop', '--connection', 'work', '--all')
            assert session.wait(timeout=10) == 143

            print('Deleting runtime files while retaining the running machine and SSH identity…', flush=True)
            cli('services', 'stop', 'work')
            supervisor.wait(timeout=10)
            shutil.rmtree(runtime)
            supervisor = start_services()
            assert identity == (state / 'client_ed25519').read_bytes()
            assert pinned_key == known_hosts.read_bytes()
            assert data('container', 'status', 'dev')['container_id'] == initial['container_id']
            tunnel()
            secrets_check()

            print('Stopping and restarting the machine; rediscovering its endpoint…', flush=True)
            shell('sh', '-c', 'touch /tmp/enclo-layer-marker')
            shell('sh', '-c', 'printf preserved > "$HOME/enclo-smoke-marker"')
            shell('sudo', '-n', 'sh', '-c', 'printf volume-preserved > /persistent/marker')
            for operation in ['remove', 'recreate']:
                refused = cli('container', operation, 'dev', check=False)
                assert refused.returncode != 0 and 'container is running' in refused.stderr
            cli('container', 'stop', 'dev')
            assert not data('container', 'status', 'dev')['running']
            wait(lambda: not data('services', 'status', 'work')['transport_ready'])
            assert supervisor.poll() is None  # unhealthy supervisor remains controllable
            restarted = data('container', 'start', 'dev')
            assert restarted['container_id'] == initial['container_id']
            assert restarted['running'] and restarted['port'] > 0
            if args.container_init:
                result = subprocess.run(['podman', '--remote=false', 'exec', restarted['container_id'], 'cat', '/persistent/container-init-count'], capture_output=True, text=True, check=True)
                assert result.stdout == 'initialized\ninitialized\n'
            report['restarted_endpoint'] = restarted
            assert identity == (state / 'client_ed25519').read_bytes()
            assert pinned_key == known_hosts.read_bytes()
            # Make the diagnostic cache deliberately wrong: live discovery must win.
            (runtime / 'containers/dev/endpoint.json').write_text('{}')
            cli('services', 'reload', 'work')
            assert shell('sh', '-c', 'cat "$HOME/enclo-smoke-marker"').stdout == 'preserved'
            tunnel()
            secrets_check()
            cli('services', 'stop', 'work')
            supervisor.wait(timeout=10)
            cli('container', 'stop', 'dev')
            assert not data('services', 'list') and not data('sessions', 'list')

            print('Recreating the test machine; checking fresh identity and host-key discovery…', flush=True)
            # Recreate applies changed config and discards only the writable layer.
            machine_config = config / 'containers/dev.toml'
            machine_config.write_text(machine_config.read_text().replace('[container]\n', '[container]\nenv=[\"ENCLO_SMOKE_REVISION=2\"]\n'))
            recreated = data('container', 'recreate', 'dev')
            supervisor = start_services()
            assert recreated['container_id'] != initial['container_id']
            if args.container_init:
                assert shell('cat', '/persistent/container-init-count').stdout == 'initialized\ninitialized\ninitialized\n'
                if args.systemd:
                    shell('test', '-f', f'/var/lib/systemd/linger/{args.ssh_user}')
                    assert shell('systemctl', 'is-active', f'user@{os.getuid()}.service').stdout.strip() == 'active'
                report['container_init'] = 'runs after home init, skips restart, reruns explicitly and on recreation'
            if args.fixed_ssh_port:
                assert recreated['port'] == fixed_port
                # No re-export: the original file must trust only the newly provisioned key.
                subprocess.run(['ssh', '-F', str(exported), 'enclo-dev', 'true'], check=True, timeout=15)
                report['stable_ssh_export'] = 'unchanged export connects after recreation'
            new_known_hosts = state / f"instances/{recreated['container_id']}/known_hosts"
            assert new_known_hosts.read_bytes().split()[1:] != pinned_key.split()[1:]
            assert identity == (state / 'client_ed25519').read_bytes()
            assert shell('test', '!', '-e', '/tmp/enclo-layer-marker').returncode == 0
            if args.managed_home:
                assert shell('sh', '-c', 'cat "$HOME/enclo-smoke-marker"').stdout == 'preserved'
                assert shell('sh', '-c', 'cat "$HOME/init-count"').stdout == 'initialized\n'
                assert shell('sh', '-c', 'stat -c %a "$HOME"').stdout.strip() == '700'
                cli('container', 'home-init', 'dev', '--rerun')
                assert shell('sh', '-c', 'cat "$HOME/init-count"').stdout == 'initialized\ninitialized\n'
            else:
                assert shell('sh', '-c', 'test ! -e \"$HOME/enclo-smoke-marker\"').returncode == 0
            assert shell('cat', '/persistent/marker').stdout == 'volume-preserved'
            if args.nested_podman:
                shell('podman', '--remote=false', 'image', 'exists', 'localhost/enclo-inner:smoke')
                shell('podman', '--remote=false', 'cp', 'inner-marker:/marker', '/tmp/inner-after-recreate')
                assert shell('cat', '/tmp/inner-after-recreate').stdout == 'nested-preserved'
                # Test execution again after recreation, using the retained local image
                # with an executable supplied from the outer container.
                shell('python3', '-c', "import pathlib,shutil,subprocess; root=pathlib.Path('/tmp/inner-root'); files=['/bin/sh']+[p for p in subprocess.check_output(['ldd','/bin/sh'],text=True).split() if p.startswith('/')]; [( (root / p.lstrip('/')).parent.mkdir(parents=True,exist_ok=True), shutil.copyfile(p,root / p.lstrip('/')), (root / p.lstrip('/')).chmod(0o755)) for p in files]")
                assert shell('podman', '--remote=false', 'run', '--rm', '--network=none', '--rootfs', '/tmp/inner-root', '/bin/sh', '-c', 'printf nested-ran').stdout == 'nested-ran'
            if args.nvidia:
                assert 'GPU' in shell('nvidia-smi', '-L').stdout
            changed = subprocess.run(['podman', '--remote=false', 'exec', recreated['container_id'], 'printenv', 'ENCLO_SMOKE_REVISION'], capture_output=True, text=True, check=True, timeout=10)
            assert changed.stdout.strip() == '2'
            tunnel()
            secrets_check()
            cli('services', 'stop', 'work')
            supervisor.wait(timeout=10)
            cli('container', 'stop', 'dev')
            cli('container', 'remove', 'dev')
            assert data('container', 'status', 'dev') is None
            assert identity == (state / 'client_ed25519').read_bytes()
            subprocess.run(['podman', '--remote=false', 'volume', 'exists', volume], check=True, timeout=10)
            report['recreated_endpoint'] = recreated
            report['result'] = 'passed'
            print(json.dumps(report, indent=2), flush=True)
        except BaseException:
            # Only read logs from this test's uniquely named container.
            subprocess.run(['podman', '--remote=false', 'logs', '--tail=60', container_name],
                           timeout=15, check=False)
            raise
        finally:
            for process in reversed(processes):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
            if tls_server:
                tls_server.shutdown()
                tls_server.server_close()
                tls_thread.join(timeout=5)
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            exists = subprocess.run(['podman', '--remote=false', 'container', 'exists', container_name],
                                    timeout=15, check=False)
            if exists.returncode == 0:
                subprocess.run(['podman', '--remote=false', 'rm', '--force', container_name],
                               timeout=60, check=True, stdout=subprocess.DEVNULL)
            elif exists.returncode != 1:
                raise RuntimeError(f'could not verify cleanup of {container_name}')
            cleanup_volumes = [volume]
            if args.managed_home:
                cleanup_volumes.append(f'enclo-home-{scope[:12]}-dev')
            for volume in cleanup_volumes:
                volume_exists = subprocess.run(['podman', '--remote=false', 'volume', 'exists', volume], timeout=15, check=False)
                if volume_exists.returncode == 0:
                    subprocess.run(['podman', '--remote=false', 'volume', 'rm', volume], timeout=30, check=True, stdout=subprocess.DEVNULL)
                elif volume_exists.returncode != 1:
                    raise RuntimeError(f'could not verify cleanup of {volume}')
            print('Removed smoke-test container, volume, state and runtime; image retained.', flush=True)


if __name__ == '__main__':
    main()
