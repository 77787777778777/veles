#!/usr/bin/env node
// Consolidated, verified-working Alibaba token-plan quota/usage reader.
// Proves out every endpoint a frontend would need. Reference implementation for the native port.
const crypto = require('crypto');
const fs = require('fs');

const acc = fs.readFileSync(process.env.HOME + '/alibaba_access', 'utf8').trim().split('\n');
const AKID = acc[0].trim().split(/\s+/)[2];
const AKSEC = acc[1].trim().split(/\s+/)[2];

const sha256hex = s => crypto.createHash('sha256').update(s, 'utf8').digest('hex');
const hmac = (k, s) => crypto.createHmac('sha256', k).update(s, 'utf8').digest('hex');
const enc = s => encodeURIComponent(s).replace(/[!'()*]/g, c => '%' + c.charCodeAt(0).toString(16).toUpperCase());

// ACS3-HMAC-SHA256, GET or POST, query params sorted+encoded.
async function acs({ method = 'GET', host, path, action, version = '2026-02-10', params = {} }) {
  const date = new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
  const q = Object.keys(params).sort().map(k => `${enc(k)}=${enc(params[k])}`).join('&');
  const ch = sha256hex('');
  const h = {
    host, 'x-acs-action': action, 'x-acs-version': version, 'x-acs-date': date,
    'x-acs-signature-nonce': crypto.randomUUID(), 'x-acs-content-sha256': ch,
    'content-type': 'application/json',
  };
  const names = Object.keys(h).filter(x => x === 'host' || x === 'content-type' || x.startsWith('x-acs-')).sort();
  const canon = [method, path, q, names.map(n => `${n}:${h[n]}`).join('\n') + '\n', names.join(';'), ch].join('\n');
  h.authorization = `ACS3-HMAC-SHA256 Credential=${AKID},SignedHeaders=${names.join(';')},Signature=${hmac(AKSEC, 'ACS3-HMAC-SHA256\n' + sha256hex(canon))}`;
  const r = await fetch(`https://${host}${path}${q ? '?' + q : ''}`, { method, headers: h });
  const text = await r.text();
  let json; try { json = JSON.parse(text); } catch { json = null; }
  return { status: r.status, json, text };
}

const HOST = 'modelstudio.ap-southeast-1.aliyuncs.com'; // also works: modelstudio.cn-beijing.aliyuncs.com

const API = {
  org:          { path: '/tokenplan/organization',                    action: 'GetOrganization' },
  orgMembers:   { path: '/tokenplan/organization/members',            action: 'ListOrganizationMembers' },
  seatStats:    { path: '/tokenplan/organization/member-seat-stats',  action: 'GetOrganizationMemberSeatStats' },
  seats:        { path: '/tokenplan/subscription/seat-detail',        action: 'GetSubscriptionSeatDetails' },
  sharedPkgs:   { path: '/tokenplan/subscription/shared-packages',    action: 'ListSubscriptionSharedPackages' },
  accounts:     { path: '/tokenplan/account',                         action: 'GetTokenPlanAccountDetail' },
  billing:      { path: '/modelstudio/billing/overview',              action: 'GetBillingOverview' },
  trend:        { path: '/modelstudio/billing/trend',                 action: 'GetBillingTrend' },
};

async function call(key, params = {}) {
  const a = API[key];
  return acs({ host: HOST, path: a.path, action: a.action, params });
}

(async () => {
  const out = {};
  for (const k of ['org', 'orgMembers', 'seatStats', 'seats', 'sharedPkgs', 'accounts']) {
    const r = await call(k);
    out[k] = { status: r.status, data: r.json };
  }

  const now = new Date();
  const billMonth = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}`;
  const mStart = `${billMonth}-01`;
  const mEnd = now.toISOString().slice(0, 10);

  out.billing = (await call('billing', { billMonth, groupBy: JSON.stringify([{ code: 'WORKSPACE_ID' }]) })).json;
  out.trend = (await call('trend', {
    granularity: 'DAY',
    timePeriod: JSON.stringify({ start: mStart, end: mEnd }),
    groupBy: JSON.stringify([{ code: 'BASE_MODEL' }]),
  })).json;

  console.log(JSON.stringify(out, null, 2));
})();