import { createPrivateKey, createPublicKey, sign } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { basename } from 'node:path';

const asset = process.argv[2];
const privateKey = process.env.WINLAUNCHPAD_UPDATE_PRIVATE_KEY;
if (!asset || !privateKey) throw new Error('缺少更新 EXE 或更新签名私钥');

// 校验私钥与已嵌入客户端的公钥匹配。签名同时绑定文件名，旧版本的签名不能用于新版本。
const key = createPrivateKey({ key: Buffer.from(privateKey.trim(), 'base64'), format: 'der', type: 'pkcs8' });
const actual = createPublicKey(key).export({ format: 'der', type: 'spki' }).subarray(-32).toString('hex');
const expected = readFileSync(new URL('../assets/update-public-key.hex', import.meta.url), 'utf8').trim();
if (key.asymmetricKeyType !== 'ed25519' || actual !== expected) throw new Error('更新签名密钥与客户端公钥不匹配');
const data = Buffer.concat([Buffer.from(`WinLaunchpad update v1\0${basename(asset)}\0`), readFileSync(asset)]);
writeFileSync(`${asset}.sig`, sign(null, data, key));
