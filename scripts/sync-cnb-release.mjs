import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { basename } from 'node:path';

const CNB_API = 'https://api.cnb.cool';
const CNB_REPOSITORY = 'crazy-figure/WinLaunchpad';
const token = process.env.CNB_TOKEN;

// CNB OpenAPI 的读写请求统一走仓库路径，允许首次发布时查不到现有 Release。
async function request(method, endpoint, body, allowNotFound = false) {
  const response = await fetch(`${CNB_API}/${CNB_REPOSITORY}/-${endpoint}`, {
    method,
    headers: {
      Accept: 'application/vnd.cnb.api+json',
      Authorization: `Bearer ${token}`,
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(60_000),
  });
  if (allowNotFound && response.status === 404) return null;
  const result = await response.text();
  if (!response.ok) throw new Error(`CNB ${method} ${endpoint} 失败（${response.status}）：${result}`);
  return result.trim() ? JSON.parse(result) : null;
}

// 标签已镜像。同步 GitHub 的发布说明和附件，重跑时按附件哈希跳过未变化的文件。
async function syncRelease(releasePath, assetPaths) {
  if (!token) throw new Error('缺少 CNB_TOKEN');
  const github = JSON.parse(await readFile(releasePath, 'utf8'));
  if (github.isDraft) throw new Error('草稿 Release 不同步');
  if (assetPaths.length === 0) throw new Error('缺少要同步的附件');

  const meta = {
    name: github.name || github.tagName,
    body: github.body || '',
    prerelease: github.isPrerelease,
  };
  const tagEndpoint = `/releases/tags/${encodeURIComponent(github.tagName)}`;
  let release = await request('GET', tagEndpoint, undefined, true);
  if (release) {
    await request('PATCH', `/releases/${release.id}`, meta);
  } else {
    await request('POST', '/releases', { tag_name: github.tagName, ...meta });
    release = await request('GET', tagEndpoint);
  }

  for (const assetPath of assetPaths) {
    await uploadAsset(release, assetPath);
  }
  console.log(`CNB Release ${github.tagName} 已同步全部附件`);
}

// 只有同名附件的 SHA-256 相同才跳过，避免把损坏的签名或可执行文件留在镜像上。
async function uploadAsset(release, assetPath) {
  const asset = await readFile(assetPath);
  const name = basename(assetPath);
  const hash = createHash('sha256').update(asset).digest('hex');
  const remote = release.assets?.find((asset) => asset.name === name);
  if (remote?.hash_algo === 'sha256' && remote.hash_value === hash) {
    console.log(`CNB 已有相同附件：${name}`);
    return;
  }

  // CNB 先签发对象存储上传地址，再通过 OpenAPI 确认。对象存储请求无需访问令牌。
  const target = await request('POST', `/releases/${release.id}/asset-upload-url`, {
    asset_name: name,
    size: asset.length,
    overwrite: true,
  });
  const verifyUrl = new URL(decodeURIComponent(target.verify_url));
  if (verifyUrl.origin !== CNB_API) throw new Error('CNB 附件确认地址无效');
  const upload = await fetch(target.upload_url, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/octet-stream' },
    body: asset,
    signal: AbortSignal.timeout(120_000),
  });
  if (!upload.ok) throw new Error(`CNB 附件上传失败（${upload.status}）`);
  const verify = await fetch(verifyUrl, {
    method: 'POST',
    headers: { Accept: 'application/vnd.cnb.api+json', Authorization: `Bearer ${token}` },
    signal: AbortSignal.timeout(60_000),
  });
  if (!verify.ok) throw new Error(`CNB 附件确认失败（${verify.status}）：${await verify.text()}`);
  console.log(`CNB 已发布 ${name}`);
}

syncRelease(process.argv[2], process.argv.slice(3)).catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
