#!/usr/bin/env python3
"""Two offline-mode Minecraft 26.2 players that play examples/local through a gateway or edge.

Packet IDs come from the repository's protocol data. The client handles offline login, compression, configuration,
keepalives, teleport and chunk acknowledgements, unsigned commands and chat components; it ignores the world.
"""
import argparse
import asyncio
import hashlib
import io
import json
from pathlib import Path
import struct
import time
import uuid
import zlib

PROTOCOL = 776
DATA = Path(__file__).resolve().parents[2] / 'crates/chunk-protocol/data/26.2'


def varint(value):
    out = bytearray()
    while value > 127:
        out.append((value & 127) | 128)
        value >>= 7
    return bytes(out + bytes([value]))


def read_varint(stream):
    value = 0
    for shift in range(0, 35, 7):
        byte = stream.read(1)
        if not byte:
            raise EOFError('incomplete VarInt')
        value |= (byte[0] & 127) << shift
        if byte[0] < 128:
            return value
    raise ValueError('invalid VarInt')


def string(value):
    data = value.encode()
    return varint(len(data)) + data


def nbt(stream, tag=None):
    """Decodes the anonymous network NBT that chat components use."""
    if tag is None:
        tag = stream.read(1)[0]

    def unpack(fmt):
        return struct.unpack('>' + fmt, stream.read(struct.calcsize('>' + fmt)))[0]

    def text():
        return stream.read(unpack('H')).decode('utf-8')

    if tag in range(1, 7):
        return unpack({1: 'b', 2: 'h', 3: 'i', 4: 'q', 5: 'f', 6: 'd'}[tag])
    if tag == 8:
        return text()
    if tag == 9:
        kind, size = unpack('B'), unpack('i')
        return [nbt(stream, kind) for _ in range(size)]
    if tag == 10:
        result = {}
        while (kind := unpack('B')) != 0:
            key = text()
            result[key] = nbt(stream, kind)
        return result
    if tag in (7, 11, 12):
        return [unpack({7: 'b', 11: 'i', 12: 'q'}[tag]) for _ in range(unpack('i'))]
    raise ValueError(f'unsupported NBT tag {tag}')


def component_text(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return ''.join(map(component_text, value))
    if isinstance(value, dict):
        return str(value.get('text', value.get('translate', ''))) + component_text(value.get('extra', []))
    return ''


class Bot:
    def __init__(self, name, address, protocol):
        self.name, self.address, self.protocol = name, address, protocol
        self.identity = uuid.UUID(bytes=hashlib.md5(('OfflinePlayer:' + name).encode()).digest(), version=3)
        self.state = 'login'
        self.compression = None
        self.messages = []
        self.plays = 0
        self.teleports = 0
        self.error = None
        self.closing = False
        self.task = None
        self.writer = None

    def log(self, event, **fields):
        print(json.dumps(dict(time=time.time(), bot=self.name, event=event, **fields)), flush=True)

    def mappings(self, direction):
        return self.protocol[self.state][direction]['types']['packet'][1][0]['type'][1]['mappings']

    def send(self, name, payload=b''):
        packet_id = next(int(k, 16) for k, v in self.mappings('toServer').items() if v == name)
        body = varint(packet_id) + payload
        if self.compression is not None:
            body = varint(len(body)) + zlib.compress(body) if len(body) >= self.compression else b'\0' + body
        self.writer.write(varint(len(body)) + body)

    def settings(self):
        self.send('settings', string('en_us') + b'\x02\x00\x01\x7f\x01\x00\x01\x00')

    async def connect(self):
        host, port = self.address
        self.reader, self.writer = await asyncio.open_connection('127.0.0.1', port)
        handshake = b'\0' + varint(PROTOCOL) + string(host) + struct.pack('>H', port) + b'\x02'
        self.writer.write(varint(len(handshake)) + handshake)
        self.send('login_start', string(self.name) + self.identity.bytes)
        self.log('connect', uuid=str(self.identity))
        self.task = asyncio.create_task(self.receive())

    async def frame(self):
        size = 0
        for shift in range(0, 35, 7):
            byte = (await self.reader.readexactly(1))[0]
            size |= (byte & 127) << shift
            if byte < 128:
                break
        else:
            raise ValueError('invalid frame length')
        if not 0 < size <= 16 * 1024 * 1024:
            raise ValueError('invalid frame size')
        body = await self.reader.readexactly(size)
        if self.compression is not None:
            buf = io.BytesIO(body)
            expected = read_varint(buf)
            body = zlib.decompress(buf.read()) if expected else buf.read()
            if expected and len(body) != expected:
                raise ValueError('invalid compressed frame')
        buf = io.BytesIO(body)
        packet_id = read_varint(buf)
        return self.mappings('toClient').get(f'0x{packet_id:02x}', f'unknown_{packet_id}'), buf

    async def receive(self):
        try:
            while True:
                name, data = await self.frame()
                if name in ('disconnect', 'kick_disconnect'):
                    reason = data.read().decode(errors='replace') if self.state == 'login' else component_text(nbt(data))
                    raise RuntimeError('server disconnect: ' + reason)
                if self.state == 'login':
                    if name == 'compress':
                        self.compression = read_varint(data)
                    elif name == 'encryption_begin':
                        raise RuntimeError('offline login unexpectedly requested encryption')
                    elif name == 'success':
                        actual = uuid.UUID(bytes=data.read(16))
                        if actual != self.identity:
                            raise RuntimeError('offline UUID mismatch')
                        self.log('login_success', uuid=str(actual))
                        self.send('login_acknowledged')
                        self.state = 'configuration'
                        self.settings()
                elif name == 'keep_alive':
                    self.send('keep_alive', data.read())
                elif name == 'ping':
                    self.send('pong', data.read())
                elif self.state == 'configuration':
                    if name == 'select_known_packs':
                        self.send('select_known_packs', b'\0')
                    elif name == 'finish_configuration':
                        self.send('finish_configuration')
                        self.state = 'play'
                        self.plays += 1
                        self.log('play', generation=self.plays)
                    elif name == 'code_of_conduct':
                        self.send('accept_code_of_conduct')
                elif name == 'start_configuration':
                    self.send('configuration_acknowledged')
                    self.state = 'configuration'
                    self.log('reconfiguration')
                    self.settings()
                elif name == 'position':
                    teleport = read_varint(data)
                    self.send('teleport_confirm', varint(teleport))
                    self.send('player_loaded')
                    self.teleports += 1
                elif name == 'chunk_batch_finished':
                    self.send('chunk_batch_received', struct.pack('>f', 20.0))
                elif name in ('system_chat', 'action_bar', 'set_title_text', 'set_title_subtitle'):
                    text = component_text(nbt(data))
                    self.messages.append(text)
                    self.log(name, text=text)
        except asyncio.CancelledError:
            pass
        except Exception as error:
            if not self.closing:
                self.error = error
                self.log('error', detail=str(error))

    async def wait(self, predicate, label, timeout=60):
        deadline = time.monotonic() + timeout
        while not predicate():
            if self.error:
                raise self.error
            if time.monotonic() > deadline:
                raise TimeoutError(f'{self.name}: {label}')
            await asyncio.sleep(0.05)
        self.log('PASS', check=label)

    async def message(self, text, since=0):
        await self.wait(lambda: any(text in item for item in self.messages[since:]), text)

    async def command(self, command, response):
        if self.state != 'play' or self.error:
            raise RuntimeError(f'{self.name}: /{command} outside healthy play')
        start = len(self.messages)
        self.log('command', text='/' + command)
        self.send('chat_command', string(command))
        await self.message(response, start)

    async def close(self):
        self.closing = True
        if self.writer:
            self.writer.close()
            await self.writer.wait_closed()
        if self.task:
            self.task.cancel()
            await self.task
        self.log('quit')


async def join(bot):
    await bot.connect()
    await bot.wait(lambda: bot.plays > 0 and bot.teleports > 0, 'play and spawn')
    await bot.message('Welcome to Lobby. Saved coins: 0.')
    await bot.message('Lobby | Coins: 0 | Visits: 1 | live')


async def scenario(address):
    assert json.loads((DATA / 'version.json').read_text())['version'] == PROTOCOL
    protocol = json.loads((DATA / 'protocol.json').read_text())
    a, b = (Bot(name, address, protocol) for name in ('SmokeBotA', 'SmokeBotB'))
    try:
        await join(a)
        await asyncio.sleep(10)
        if a.error or a.state != 'play':
            raise RuntimeError('A did not stay in healthy play for 10 seconds')
        a.log('PASS', check='stayed in play for 10 seconds')
        await join(b)
        await a.command('population', 'This lobby has 2 player(s).')
        await a.command('hello smoke-hello-A', 'smoke-hello-A')
        await a.command('coin', 'Lobby | Coins: 1 | Visits: 1 | live')
        await b.command('coin', 'Lobby | Coins: 1 | Visits: 1 | live')
        generation = a.plays
        await a.command('travel arena', 'Welcome to Arena (16 slots). Saved coins: 1.')
        await a.wait(lambda: a.plays > generation, 'travel re-entered play')
        await a.message('Arena (16 slots) | Coins: 1 | Visits: 2 | live')
        await a.command('coin', 'Arena (16 slots) | Coins: 2 | Visits: 2 | live')
        await b.command('population', 'This lobby has 1 player(s).')
        if a.error or b.error:
            raise RuntimeError('a bot connection failed before quitting')
        await a.close()
        await b.close()
        print('PASS: complete two-player bot scenario', flush=True)
    finally:
        for bot in (a, b):
            if not bot.closing:
                await bot.close()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, required=True, help='the loopback port to connect to')
    parser.add_argument('--host', default='localhost', help='the hostname the handshake names')
    args = parser.parse_args()
    if args.port == 25565:
        parser.error('port 25565 is reserved for a real server')
    asyncio.run(scenario((args.host, args.port)))
