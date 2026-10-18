'use strict';
const $ = id => document.getElementById(id);
const form = $('settings');
let readerNames = [], initialized = false, polling = false;
function message(text, error = false) { $('message').textContent = text; $('message').classList.toggle('error', error); }
async function api(path, method = 'GET', data) {
  const headers = {};
  if (data !== undefined) headers['Content-Type'] = 'application/json';
  const response = await fetch('/api/' + path, {method, headers, body:data === undefined ? undefined : JSON.stringify(data), cache:'no-store', signal:AbortSignal.timeout(15000)});
  const result = await response.json();
  if (!response.ok) throw new Error(result.error || 'HTTP ' + response.status);
  return result;
}
function option(select, value, text) { const item=document.createElement('option'); item.value=value; item.textContent=text; select.append(item); }
function choices(select, selected, auto = true) {
  select.replaceChildren();
  if (auto) option(select,'','自動');
  for (const name of new Set([...readerNames, ...(selected ? [selected] : [])])) option(select,name,name);
  select.value = selected;
}
function fixedRow(port = '', reader = '') {
  const row = document.createElement('div'); row.className='fixed-row';
  const label = document.createElement('label'); label.textContent='ポート';
  const input=document.createElement('input'); input.type='number'; input.min='1'; input.max='65535'; input.required=true; input.value=port; label.append(input);
  const target=document.createElement('label'); target.textContent='リーダー';
  const select=document.createElement('select'); select.required=true; choices(select,reader,false); target.append(select);
  const remove=document.createElement('button'); remove.type='button'; remove.textContent='削除'; remove.onclick=()=>row.remove();
  row.append(label,target,remove); $('fixed').append(row);
}
function fill(settings) {
  form.elements.address.value=settings.address; form.elements.port.value=settings.port;
  form.elements.allow.value=settings.allow.join('\n'); form.elements.verbose.checked=settings.verbose;
  choices(form.elements.bcas,settings.pins[0]); choices(form.elements.acas,settings.pins[1]);
  $('fixed').replaceChildren(); settings.fixed.forEach(f=>fixedRow(f.port,f.reader));
}
function renderItems(target, items) {
  target.replaceChildren();
  for (const text of items.length ? items : ['なし']) { const item=document.createElement('div'); item.className='item'; item.textContent=text; target.append(item); }
}
async function poll() {
  if (polling) return;
  polling=true;
  try {
    const data=await api('state'), s=data.snapshot;
    const changedReaders = JSON.stringify(readerNames) !== JSON.stringify(s.readers);
    readerNames=s.readers;
    if (initialized && changedReaders) {
      for (const select of [form.elements.bcas, form.elements.acas]) choices(select,select.value);
      for (const select of $('fixed').querySelectorAll('select')) choices(select,select.value,false);
    }
    $('version').textContent='v'+data.version;
    $('connection').textContent='・'+Object.keys(s.clients).length+' クライアント';
    $('listen').textContent='TCP '+data.settings.address+':'+data.settings.port+' ／ Web '+data.web_listen;
    renderItems($('clients'),Object.values(s.clients).map(c=>c.peer+' ／ '+new Date(c.connected_at*1000).toLocaleString('ja-JP')+' ／ '+(c.reader||'待機中')));
    renderItems($('readers'),s.readers.length ? s.readers.map(name=>{const p=s.classifications[name]; return name+' ／ '+(!p?'未判定':!p.openable?'利用不可':p.acas?'ACAS':p.bcas?'B-CAS':'その他');}) : ['未検出。「判定し直す」でリーダーを列挙・判定します。']);
    $('routes').textContent='B-CAS: '+(s.routes[0]||'なし')+'\nACAS: '+(s.routes[1]||'なし')+'\n'+data.settings.fixed.map(f=>'固定 '+f.port+': '+f.reader+' → '+(s.fixed_routes[f.reader]||'未解決')).join('\n');
    const logs=$('logs'), bottom=logs.scrollHeight-logs.scrollTop-logs.clientHeight<40;
    logs.textContent=s.logs.join('\n'); if(bottom) logs.scrollTop=logs.scrollHeight;
    if (!initialized) {
      const config=await api('config'); fill(config.settings);
      $('path').textContent='保存先: '+config.config_path;
      $('overrides').textContent='CLI 優先項目: '+(config.cli_overrides.join(', ')||'なし');
      initialized=true;
      if ($('message').textContent === '接続中…') message('接続しました。');
    }
  } catch(e) { message(e.message,true); } finally { polling=false; }
}
$('add').onclick=()=>fixedRow('',readerNames[0]||'');
$('refresh').onclick=async()=>{const button=$('refresh');button.disabled=true;try{await api('refresh','POST');message('再判定が完了しました。');await poll();}catch(e){message(e.message,true);}finally{button.disabled=false;}};
form.onsubmit=async event=>{
  event.preventDefault();
  const fixed=[...$('fixed').children].map(row=>({port:Number(row.querySelector('input').value),reader:row.querySelector('select').value}));
  const port=Number(form.elements.port.value), ports=[port,...fixed.map(f=>f.port)];
  if (ports.some(p=>!Number.isInteger(p)||p<1||p>65535) || new Set(ports).size!==ports.length) {message('ポートは 1〜65535 の範囲で、重複しない値を指定してください。',true);return;}
  const settings={address:form.elements.address.value.trim(),port,allow:form.elements.allow.value.split('\n').map(s=>s.trim()).filter(Boolean),pins:[form.elements.bcas.value,form.elements.acas.value],fixed,verbose:form.elements.verbose.checked};
  const button=form.querySelector('[type=submit]');button.disabled=true;
  try{await api('config','PUT',settings);message('保存して反映しました。接続中のクライアントは維持しています。');await poll();}catch(e){message(e.message,true);}finally{button.disabled=false;}
};
poll();setInterval(poll,2000);
