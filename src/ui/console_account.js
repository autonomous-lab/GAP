(() => {
  const byId = id => document.getElementById(id);
  const account = byId('account');
  const fleet = byId('fleetSection');
  const cards = document.createElement('div');
  cards.id = 'vmCards';
  cards.setAttribute('aria-live', 'polite');
  fleet.insertBefore(cards, fleet.querySelector('div[style*="overflow"]'));
  byId('overviewStats').after(fleet);
  fleet.querySelector('h2').textContent = 'Your MicroVMs';
  fleet.querySelector('p').textContent = 'Open a machine to connect, publish an app, or manage spending.';
  byId('placeVM').textContent = 'Create a MicroVM';
  const sessionCopy = byId('login').querySelector('small');
  if (sessionCopy) sessionCopy.textContent = 'Your session can be restored for up to 48 hours on this browser. Sign out to end it.';

  const advanced = document.createElement('details');
  advanced.className = 'console-advanced';
  const summary = document.createElement('summary');
  summary.textContent = 'Advanced';
  advanced.append(summary);
  const nav = document.querySelector('.console-links');
  for (const view of ['access', 'activity']) {
    const link = nav.querySelector(`[data-view="${view}"]`);
    if (link) advanced.append(link);
  }
  nav.after(advanced);
  viewNames.overview = 'Your workspace';
  viewNames.machines = 'Your MicroVMs';
  viewNames.billing = 'Billing';
  viewNames.serverless = 'Serverless';
  const serverlessLink = document.createElement('a');
  serverlessLink.href = '#serverless';
  serverlessLink.dataset.view = 'serverless';
  serverlessLink.innerHTML = '<span aria-hidden="true">{ }</span>Serverless';
  nav.querySelector('[data-view="projects"]').after(serverlessLink);
  const serverlessSection = document.createElement('section');
  serverlessSection.id = 'serverlessSection';
  serverlessSection.innerHTML = '<div class="eyebrow">Across your projects</div><h2>Functions, sites and data</h2><p class="project-intro">Your serverless resources live inside projects. Open a project to deploy a function or manage its resources.</p><div id="serverlessCards" class="project-grid" aria-live="polite"></div>';
  byId('projectsSection').after(serverlessSection);

  const projectHome = document.createElement('div');
  projectHome.id = 'projectHome';
  byId('projectsSection').append(projectHome);
  const projectNumber = p => [...allProjects].sort((a, b) => a.id.localeCompare(b.id)).findIndex(x => x.id === p.id) + 1;
  const projectName = p => p.name || `Project ${projectNumber(p)}`;
  const projectRows = p => fleetRows.filter(row => row.p.id === p.id && row.vm);
  const projectRoute = () => /^#project\/(prj_[0-9a-f]{24})$/.exec(location.hash)?.[1];
  const overviewCache = new Map();
  const periodCache = new Map();
  const credits = value => (Number(value || 0) / 1000000).toLocaleString('en-US', {minimumFractionDigits:2, maximumFractionDigits:4});
  function summaryCard(title, detail, href, action) {
    const card = document.createElement('article'); card.className = 'resource-card';
    const h = document.createElement('h4'); h.textContent = title;
    const p = document.createElement('p'); p.textContent = detail;
    card.append(h, p);
    if (href) { const link = document.createElement('a'); link.href = href; link.textContent = action || 'Open →'; card.append(link); }
    return card;
  }
  async function projectOverview(p, refresh = false) {
    const cached = overviewCache.get(p.id);
    if (!refresh && cached && Date.now() - cached.at < 30000) return cached.promise;
    const promise = (async () => {
      const capability = await api('project-token', {project_id:p.id,node_id:p.node,ttl_seconds:300});
      return nodeRequest(p, capability.token, '/overview');
    })();
    overviewCache.set(p.id,{at:Date.now(),promise});
    promise.catch(() => { if (overviewCache.get(p.id)?.promise === promise) overviewCache.delete(p.id); });
    return promise;
  }
  async function projectUsage(p) {
    const key = p.id + ':7';
    const cached = periodCache.get(key);
    if (cached && Date.now() - cached.at < 30000) return cached.promise;
    const promise = api('usage?days=7&project_id=' + encodeURIComponent(p.id));
    periodCache.set(key,{at:Date.now(),promise});
    promise.catch(() => { if (periodCache.get(key)?.promise === promise) periodCache.delete(key); });
    return promise;
  }
  async function projectCall(p, path, method, body) {
    const capability = await api('project-token',{project_id:p.id,node_id:p.node,ttl_seconds:300});
    const response = await fetch(nodePath(p.node) + '/v1/cloud/projects/' + p.id + path, {
      method, credentials:'same-origin', cache:'no-store',
      headers:{'Content-Type':'application/json',Authorization:'Bearer '+capability.token},
      body:body === undefined ? undefined : JSON.stringify(body),
    });
    const result = await response.json();
    if (!response.ok) throw Error(result.error?.message || result.error?.code || 'Resource request failed');
    return result;
  }
  function modal(titleText,description) {
    const dialog = document.createElement('dialog'); dialog.className = 'function-dialog';
    const form = document.createElement('form');
    const title = document.createElement('h2'); title.textContent = titleText;
    const intro = document.createElement('p'); intro.textContent = description;
    const status = document.createElement('p'); status.setAttribute('role','status');
    const cancel = document.createElement('button'); cancel.type = 'button'; cancel.textContent = 'Close'; cancel.onclick = () => dialog.close();
    form.append(title,intro); dialog.append(form); document.body.append(dialog);
    dialog.addEventListener('close',()=>dialog.remove());
    return {dialog,form,status,cancel};
  }
  function invokeDialog(p,name) {
    const {dialog,form,status,cancel} = modal('Test '+name,'Send JSON to the active function version. This call uses your project management permission.');
    const label = document.createElement('label'); label.textContent = 'Request JSON';
    const input = document.createElement('textarea'); input.value = '{}'; label.append(input);
    const output = document.createElement('pre'); output.className = 'resource-output';
    const submit = document.createElement('button'); submit.className = 'primary'; submit.textContent = 'Run function';
    form.append(label,status,submit,cancel,output);
    form.onsubmit = run(async () => {
      submit.disabled = true;
      try { const request = JSON.parse(input.value); const result = await projectCall(p,'/functions/'+encodeURIComponent(name)+'/invoke','POST',{request}); output.textContent = JSON.stringify(result.result,null,2); status.textContent = 'Version '+result.version+' completed.'; }
      catch (error) { status.textContent = error.message; }
      finally { submit.disabled = false; }
    });
    dialog.showModal();
  }
  function siteDialog(p,site) {
    const {dialog,form,status,cancel} = modal('Publish a site','Publish a single HTML page as a new immutable version. Your GAP site remains protected by Basic Auth.');
    const userLabel = document.createElement('label'); userLabel.textContent = 'Basic Auth username';
    const username = document.createElement('input'); username.required = true; username.value = site?.username || 'preview'; userLabel.append(username);
    const passLabel = document.createElement('label'); passLabel.textContent = site ? 'New password, 12–128 bytes (leave blank to keep current)' : 'Basic Auth password, 12–128 bytes';
    const password = document.createElement('input'); password.type = 'password'; password.autocomplete = 'new-password'; password.required = !site; password.minLength = 12; passLabel.append(password);
    const htmlLabel = document.createElement('label'); htmlLabel.textContent = 'index.html';
    const source = document.createElement('textarea'); source.required = true; source.value = '<!doctype html>\n<html lang="en"><meta charset="utf-8"><title>Hello</title><h1>Hello from GAP</h1></html>'; htmlLabel.append(source);
    const publish = document.createElement('button'); publish.className = 'primary'; publish.textContent = 'Publish new version';
    form.append(userLabel,passLabel,htmlLabel,status,publish,cancel);
    form.onsubmit = run(async () => {
      publish.disabled = true;
      try {
        const config = {enabled:true,entrypoint:'index.html',spa_fallback:false,auth:{mode:'basic',username:username.value}};
        if (password.value) config.auth.password = password.value;
        status.textContent = 'Configuring site…';
        await projectCall(p,'/site','PUT',config);
        const version = await projectCall(p,'/site/versions','POST',{});
        status.textContent = 'Uploading version '+version.version+'…';
        await projectCall(p,'/site/versions/'+version.version+'/files/index.html','PUT',{content_base64:encodeText(source.value)});
        await projectCall(p,'/site/versions/'+version.version+'/activate','POST',{});
        overviewCache.delete(p.id); dialog.close(); renderProjects(); say('Site version '+version.version+' published.');
      } catch (error) { status.textContent = error.message; publish.disabled = false; }
    });
    dialog.showModal();
  }
  function databaseDialog(p) {
    const {dialog,form,status,cancel} = modal('Explore database','Read-only SQL queries against this project’s SQLite database. Use parameterized API calls for application writes.');
    const label = document.createElement('label'); label.textContent = 'SQL query';
    const sql = document.createElement('textarea'); sql.value = 'SELECT name, type FROM sqlite_schema ORDER BY name LIMIT 50'; label.append(sql);
    const output = document.createElement('pre'); output.className = 'resource-output';
    const submit = document.createElement('button'); submit.className = 'primary'; submit.textContent = 'Run query';
    form.append(label,status,submit,cancel,output);
    form.onsubmit = run(async () => {
      submit.disabled = true;
      try { const result = await projectCall(p,'/database/query','POST',{sql:sql.value,params:[]}); output.textContent = JSON.stringify({columns:result.columns,rows:result.rows,truncated:result.truncated},null,2); status.textContent = result.rows.length+' row(s).'; }
      catch (error) { status.textContent = error.message; }
      finally { submit.disabled = false; }
    });
    dialog.showModal();
  }
  function encodeText(value) {
    const bytes = new TextEncoder().encode(value);
    let binary = ''; for (let i=0;i<bytes.length;i+=8192) binary += String.fromCharCode(...bytes.subarray(i,i+8192));
    return btoa(binary);
  }
  function storageDialog(p,kind) {
    const object = kind === 'objects';
    const {dialog,form,status,cancel} = modal(object ? 'Text objects' : 'Key–value storage', object ? 'Read or save a UTF-8 text object by key. Binary uploads remain available through the API.' : 'Read or save a UTF-8 value by key. Values can also be managed through the project API.');
    const keyLabel = document.createElement('label'); keyLabel.textContent = 'Key';
    const key = document.createElement('input'); key.required = true; key.placeholder = object ? 'notes/readme.txt' : 'message'; keyLabel.append(key);
    const valueLabel = document.createElement('label'); valueLabel.textContent = 'Text value';
    const value = document.createElement('textarea'); valueLabel.append(value);
    const read = document.createElement('button'); read.type = 'button'; read.textContent = 'Read key';
    const save = document.createElement('button'); save.className = 'primary'; save.textContent = 'Save';
    read.onclick = run(async () => {
      if (!key.value) { status.textContent = 'Enter a key first.'; return; }
      const result = await projectCall(p,'/'+kind+'/'+encodeURIComponent(key.value),'GET');
      value.value = result.found ? new TextDecoder().decode(Uint8Array.from(atob(object ? result.content_base64 : result.value_base64),c => c.charCodeAt(0))) : '';
      status.textContent = result.found ? 'Value loaded.' : 'No value exists for this key.';
    });
    form.append(keyLabel,valueLabel,status,read,save,cancel);
    form.onsubmit = run(async () => {
      save.disabled = true;
      try {
        const body = object ? {content_base64:encodeText(value.value),media_type:'text/plain; charset=utf-8'} : {value_base64:encodeText(value.value)};
        await projectCall(p,'/'+kind+'/'+encodeURIComponent(key.value),'PUT',body);
        overviewCache.delete(p.id); status.textContent = 'Saved.'; renderProjects();
      } catch (error) { status.textContent = error.message; }
      finally { save.disabled = false; }
    });
    dialog.showModal();
  }
  function functionDialog(p) {
    const dialog = document.createElement('dialog'); dialog.className = 'function-dialog';
    const form = document.createElement('form');
    const title = document.createElement('h2'); title.textContent = 'Deploy a function';
    const explanation = document.createElement('p'); explanation.textContent = 'Create a JavaScript function in ' + projectName(p) + '. Publishing creates a version; activating makes it live.';
    const nameLabel = document.createElement('label'); nameLabel.textContent = 'Function name';
    const name = document.createElement('input'); name.required = true; name.pattern = '[A-Za-z][A-Za-z0-9_-]{0,63}'; name.placeholder = 'hello'; nameLabel.append(name);
    const sourceLabel = document.createElement('label'); sourceLabel.textContent = 'JavaScript source';
    const source = document.createElement('textarea'); source.required = true; source.spellcheck = false; source.value = '({ request }) => ({ message: "Hello from GAP" })'; sourceLabel.append(source);
    const status = document.createElement('p'); status.setAttribute('role','status');
    const deploy = document.createElement('button'); deploy.className = 'primary'; deploy.textContent = 'Deploy and activate';
    const cancel = document.createElement('button'); cancel.type = 'button'; cancel.textContent = 'Cancel'; cancel.onclick = () => dialog.close();
    form.append(title,explanation,nameLabel,sourceLabel,status,deploy,cancel);
    form.onsubmit = run(async () => {
      deploy.disabled = true;
      try {
        const capability = await api('project-token',{project_id:p.id,node_id:p.node,ttl_seconds:300});
        status.textContent = 'Publishing…';
        const version = await nodeRequest(p,capability.token,'/functions/'+encodeURIComponent(name.value),{runtime:'javascript',source:source.value});
        await nodeRequest(p,capability.token,'/functions/'+encodeURIComponent(name.value)+'/activate',{version:version.version});
        overviewCache.delete(p.id); dialog.close(); renderProjects();
        say('Function '+name.value+' deployed and activated.');
      } catch (error) { status.textContent = error.message; deploy.disabled = false; }
    });
    dialog.append(form); document.body.append(dialog); dialog.addEventListener('close',()=>dialog.remove()); dialog.showModal();
  }
  async function fillProjectResources(p, mount) {
    mount.textContent = 'Loading project resources and usage…';
    try {
      const [overview, usage, realtime] = await Promise.all([
        p.role === 'viewer' ? Promise.resolve({resources:{}}) : projectOverview(p),
        projectUsage(p),
        p.role === 'viewer' ? Promise.resolve(null) : projectCall(p,'/realtime/credits','GET').catch(() => null),
      ]);
      if (!mount.isConnected) return;
      const resources = overview.resources || {};
      mount.replaceChildren();
      const heading = document.createElement('h4'); heading.textContent = 'Resources'; mount.append(heading);
      const grid = document.createElement('div'); grid.className = 'project-resource-grid';
      const functions = resources.functions || [];
      const fnCard = summaryCard('Functions', p.role === 'viewer' ? 'Serverless inventory requires management access' : functions.length ? functions.length + ' deployed' : 'No functions yet');
      for (const fn of functions) {
        const line = document.createElement('div'); line.className = 'resource-line';
        const link = document.createElement('button'); link.type = 'button'; link.className = 'resource-link'; link.textContent = fn.name + ' →'; link.onclick = () => invokeDialog(p,fn.name);
        const state = document.createElement('span'); state.textContent = fn.active_version ? 'Live · v' + fn.active_version : 'Not active';
        line.append(link,state); fnCard.append(line);
      }
      if (p.role !== 'viewer') { const button = document.createElement('button'); button.textContent = 'Deploy function'; button.onclick = () => functionDialog(p); fnCard.append(button); }
      grid.append(fnCard);
      const site = resources.site;
      const liveSite = site?.enabled && site?.active_version;
      const siteCard = summaryCard('Site', p.role === 'viewer' ? 'Management access required' : liveSite ? 'Published · version '+site.active_version : 'No active release', p.role === 'viewer' ? null : liveSite ? nodePath(p.node) + '/sites/' + p.id + '/' : null, liveSite ? 'Visit site ↗' : null);
      if (p.role !== 'viewer') { const publish = document.createElement('button'); publish.textContent = liveSite ? 'Publish new version' : 'Publish site'; publish.onclick = () => siteDialog(p,site); siteCard.append(publish); }
      grid.append(siteCard);
      const tables = resources.database_schema?.rows?.length || 0;
      const database = summaryCard('Database', p.role === 'viewer' ? 'Management access required' : tables ? tables + ' schema objects' : 'No tables yet');
      if (p.role !== 'viewer') { const explore = document.createElement('button'); explore.textContent = 'Explore data'; explore.onclick = () => databaseDialog(p); database.append(explore); }
      grid.append(database);
      const kv = summaryCard('Key–value', p.role === 'viewer' ? 'Management access required' : (resources.kv_count || 0) + ' keys');
      if (p.role !== 'viewer') { const manage = document.createElement('button'); manage.textContent = 'Read or save a key'; manage.onclick = () => storageDialog(p,'kv'); kv.append(manage); }
      grid.append(kv);
      const objects = summaryCard('Objects', p.role === 'viewer' ? 'Management access required' : (resources.object_count || 0) + ' stored objects');
      if (p.role !== 'viewer') { const manage = document.createElement('button'); manage.textContent = 'Read or save text'; manage.onclick = () => storageDialog(p,'objects'); objects.append(manage); }
      grid.append(objects);
      const schedules = resources.schedules || [];
      grid.append(summaryCard('Schedules', p.role === 'viewer' ? 'Management access required' : schedules.length + ' scheduled job' + (schedules.length === 1 ? '' : 's'), p.role === 'viewer' ? null : '/docs', 'Scheduling API →'));
      grid.append(summaryCard('Realtime', p.role === 'viewer' ? 'Management access required' : realtime ? realtime.account.balance + ' separate realtime credits · ' + realtime.account.spent_total + ' used' : 'Realtime balance unavailable', p.role === 'viewer' ? null : '/docs', 'Realtime API →'));
      const spend = summaryCard('MicroVM usage · last 7 complete days', credits(usage.usage?.debited_microcredits) + ' wallet credits charged' + (usage.coverage_complete ? '' : ' · partial data'));
      const note = document.createElement('small'); note.textContent = 'CPU '+Math.round((usage.usage?.vcpu_ms||0)/3600000)+' vCPU-h · Network '+(((usage.usage?.bytes_out||0)+(usage.usage?.bytes_in||0))/1073741824).toFixed(2)+' GiB'; spend.append(note);
      grid.append(spend); mount.append(grid);
    } catch (error) { if (mount.isConnected) mount.textContent = 'Resources unavailable: ' + error.message; }
  }

  function renderProjects() {
    const current = projectRoute();
    projectHome.replaceChildren();
    if (current) {
      const p = allProjects.find(x => x.id === current);
      if (!p) {
        const missing = document.createElement('p');
        missing.textContent = 'This project is not available in your account.';
        projectHome.append(missing);
        return;
      }
      const back = document.createElement('a');
      back.href = '#projects';
      back.className = 'project-back';
      back.textContent = '← All projects';
      const heading = document.createElement('div');
      heading.className = 'project-detail-head';
      const title = document.createElement('h3');
      title.textContent = projectName(p);
      const summary = document.createElement('p');
      summary.textContent = `${p.node} · ${projectRows(p).length} MicroVM${projectRows(p).length === 1 ? '' : 's'} · ${p.role} access`;
      heading.append(title, summary);
      if (p.role === 'owner' || p.role === 'operator') {
        const rename = document.createElement('button'); rename.className = 'rename-project'; rename.textContent = 'Rename';
        rename.onclick = () => {
          const form = document.createElement('form'); form.className = 'rename-form';
          const input = document.createElement('input'); input.value = projectName(p); input.maxLength = 64; input.required = true; input.setAttribute('aria-label','Project name');
          const save = document.createElement('button'); save.textContent = 'Save'; save.className = 'primary';
          const cancel = document.createElement('button'); cancel.type = 'button'; cancel.textContent = 'Cancel'; cancel.onclick = () => { form.remove(); rename.disabled = false; };
          form.append(input,save,cancel); heading.after(form); input.focus(); input.select(); rename.disabled = true;
          form.onsubmit = run(async () => {
            const proposed = input.value.trim();
            if (!proposed || proposed === projectName(p)) { form.remove(); rename.disabled = false; return; }
            const result = await api('project-name',{request_id:crypto.randomUUID(),project_id:p.id,name:proposed});
            p.name = result.name; renderProjects(); showView();
          });
        };
        heading.append(rename);
      }
      const actions = document.createElement('div');
      actions.className = 'project-detail-actions';
      if (p.role !== 'viewer') {
        const create = document.createElement('button');
        create.className = 'primary';
        create.textContent = 'Create a MicroVM';
        create.onclick = run(() => placementDialog(p.id));
        actions.append(create);
      }
      const machines = document.createElement('div');
      machines.className = 'project-machine-list';
      const rows = projectRows(p);
      if (!rows.length) {
        const empty = document.createElement('p');
        empty.textContent = 'No MicroVM in this project yet. Create one here, or return to your machines.';
        machines.append(empty);
      }
      for (const row of rows) {
        const button = document.createElement('button');
        button.className = 'project-machine';
        button.textContent = `MicroVM ${row.vm.vm_id.slice(-8)} · ${row.vm.state} · ${row.p.node}  →`;
        button.onclick = run(async () => {
          if (p.role !== 'viewer') return openMachine(row.p, row.vm.vm_id, false);
          await details(row);
          let info = machines.querySelector('.vm-card-details');
          if (!info) { info = document.createElement('pre'); info.className = 'vm-card-details'; machines.append(info); }
          info.textContent = byId('fleetDetails').textContent;
        });
        machines.append(button);
      }
      const resources = document.createElement('div'); resources.className = 'project-resources';
      void fillProjectResources(p,resources);
      const technical = document.createElement('details');
      technical.className = 'project-technical';
      const technicalSummary = document.createElement('summary');
      technicalSummary.textContent = 'Technical details and API access';
      const id = document.createElement('code');
      id.textContent = p.id;
      const apiButton = document.createElement('button');
      apiButton.textContent = 'Generate a temporary API token';
      const tokenOutput = document.createElement('p');
      tokenOutput.className = 'project-token-output';
      apiButton.onclick = run(async () => {
        const result = await api('project-token', {project_id:p.id});
        tokenOutput.textContent = `Expires in 2 minutes: ${result.token}`;
        setTimeout(() => { tokenOutput.textContent = ''; }, 120000);
      });
      technical.append(technicalSummary, id, apiButton, tokenOutput);
      projectHome.append(back, heading, actions, machines, resources, technical);
      return;
    }
    const intro = document.createElement('p');
    intro.className = 'project-intro';
    intro.textContent = 'A project holds your MicroVMs, functions, site, data and spending in one place.';
    const grid = document.createElement('div');
    grid.className = 'project-grid';
    for (const p of allProjects) {
      const item = document.createElement('article');
      item.className = 'project-card';
      const title = document.createElement('h3');
      title.textContent = projectName(p);
      const description = document.createElement('p');
      description.textContent = `${p.node} · ${projectRows(p).length} MicroVM${projectRows(p).length === 1 ? '' : 's'} · serverless · ${p.role} access`;
      const open = document.createElement('a');
      open.href = '#project/' + p.id;
      open.textContent = 'Open project →';
      item.append(title, description, open);
      grid.append(item);
    }
    projectHome.append(intro, grid);
  }

  async function renderServerless() {
    const grid = byId('serverlessCards');
    grid.textContent = 'Loading serverless resources…';
    const cards = await Promise.all(allProjects.map(async p => {
      if (p.role === 'viewer') return summaryCard(projectName(p), 'Viewer access · serverless inventory unavailable', '#project/' + p.id, 'Open project →');
      try {
        const value = await projectOverview(p);
        const resources = value.resources || {};
        const count = (resources.functions || []).length;
        const card = summaryCard(projectName(p), count + ' function' + (count === 1 ? '' : 's') + ' · ' + (resources.site?.active_version ? 'site live' : 'no site') + ' · ' + (resources.database_schema?.rows?.length || 0) + ' DB objects · ' + (resources.kv_count || 0) + ' KV keys', '#project/' + p.id, 'Manage resources →');
        return card;
      } catch (error) { return summaryCard(projectName(p), 'Unavailable · ' + error.message, '#project/' + p.id, 'Open project →'); }
    }));
    if (location.hash === '#serverless') grid.replaceChildren(...cards);
  }

  const originalPlacementDialog = placementDialog;
  placementDialog = function (preferredProject) {
    originalPlacementDialog();
    const dialog = document.querySelector('dialog:last-of-type');
    const description = dialog.querySelector('.section-intro');
    description.textContent = 'Start with 0.5 vCPU, 512 MiB RAM and 8 GiB disk. Customize the size or SSH key only if you need to.';
    const labels = [...dialog.querySelectorAll('label')];
    const projectLabel = labels.find(label => label.firstChild?.textContent === 'Project');
    if (projectLabel) {
      const select = projectLabel.querySelector('select');
      for (const option of select.options) {
        const p = allProjects.find(x => x.id === option.value);
        if (p) option.textContent = `${projectName(p)} · ${p.node}`;
      }
      if (preferredProject && [...select.options].some(option => option.value === preferredProject)) select.value = preferredProject;
      projectLabel.firstChild.textContent = 'Add to project';
    }
    dialog.querySelector('details summary').textContent = 'Advanced: size and SSH key';
  };

  function card(row) {
    const item = document.createElement('article');
    item.className = 'vm-card' + (row.error ? ' vm-card-error' : '');
    const head = document.createElement('div');
    head.className = 'vm-card-head';
    const icon = document.createElement('span');
    icon.className = 'vm-card-icon';
    icon.setAttribute('aria-hidden', 'true');
    icon.textContent = '▣';
    const title = document.createElement('div');
    const name = document.createElement('h3');
    name.textContent = row.vm ? 'MicroVM ' + row.vm.vm_id.slice(-8) : 'New MicroVM';
    const sub = document.createElement('div');
    sub.className = 'vm-card-sub';
    sub.textContent = projectName(row.p) + ' · ' + row.p.node;
    title.append(name, sub);
    head.append(icon, title);
    const state = document.createElement('span');
    state.className = 'state-pill';
    state.dataset.state = row.error ? 'error' : row.vm?.state || 'empty';
    state.textContent = row.error ? 'Host unavailable' : row.migration ? 'Migrating' : row.vm?.state || 'Ready to create';
    const description = document.createElement('p');
    description.className = 'vm-card-summary';
    description.textContent = row.error ? 'We could not load this project. Retry the inventory.' : row.vm
      ? `${row.vm.vcpus} vCPU · ${row.vm.memory_mib} MiB RAM · ${row.vm.disk_gib} GiB disk`
      : 'Choose resources and start your first workload.';
    const actions = document.createElement('div');
    actions.className = 'vm-card-actions';
    const button = document.createElement('button');
    button.type = 'button';
    const readOnly = row.p.role === 'viewer';
    button.textContent = row.error ? 'Retry' : readOnly ? 'View connection' : row.vm ? 'Open MicroVM →' : 'Create MicroVM →';
    button.disabled = readOnly && !row.vm;
    button.onclick = run(async () => {
      if (row.error) return loadFleet();
      if (!readOnly) return openMachine(row.p, row.vm?.vm_id, !row.vm);
      await details(row);
      let info = item.querySelector('.vm-card-details');
      if (!info) { info = document.createElement('pre'); info.className = 'vm-card-details'; item.append(info); }
      info.textContent = byId('fleetDetails').textContent;
    });
    actions.append(button);
    item.append(head, state, description, actions);
    return item;
  }

  function renderCards() {
    const create = byId('placeVM');
    create.textContent = allProjects.length ? 'Create a MicroVM' : 'Create your first project';
    create.onclick = allProjects.length ? run(() => placementDialog()) : () => { location.assign('/signup'); };
    const rows = fleetRows.filter(row => row.vm && (!byId('fleetNode').value || row.p.node === byId('fleetNode').value)
      && (!byId('fleetProject').value || row.p.id === byId('fleetProject').value));
    cards.replaceChildren();
    if (!rows.length) {
      const empty = document.createElement('p');
      empty.className = 'vm-empty';
      empty.textContent = allProjects.length
        ? 'No MicroVM yet. Create one to start; existing empty projects are listed under Projects.'
        : 'A project is needed before your first MicroVM. Create one to get started.';
      cards.append(empty);
      return;
    }
    for (const row of rows) cards.append(card(row));
  }
  const originalDrawFleet = drawFleet;
  drawFleet = function () { originalDrawFleet(); renderCards(); renderProjects(); };

  const accessPanel = byId('memberSection');
  accessPanel.querySelector('p').textContent = 'Your own access is automatic. Add another agent only when you want to collaborate on a project.';
  const accessAdvanced = document.createElement('details');
  accessAdvanced.className = 'access-advanced';
  const accessSummary = document.createElement('summary');
  accessSummary.textContent = 'Manage agents and permissions';
  accessAdvanced.append(accessSummary, byId('memberForm'), byId('members'));
  accessPanel.append(accessAdvanced);

  const originalLoadMigrations = loadMigrations;
  loadMigrations = async function () {
    await originalLoadMigrations();
    migrationTitle.textContent = 'Recent MicroVM moves';
    for (const [index, move] of migrationJobs.entries()) {
      const line = migrationList.children[index];
      if (!line?.firstChild) continue;
      const machine = `MicroVM ${move.vm_id.slice(-8)}`;
      const path = `${move.source_node} → ${move.target_node}`;
      const progress = move.job.total && move.job.status === 'running'
        ? ` · ${Math.round(100 * move.job.received / move.job.total)}% transferred` : '';
      line.firstChild.textContent = move.job.status === 'failed'
        ? `${machine} · move failed (${path})${move.job.error ? ': ' + move.job.error : ''} `
        : move.phase === 'committed' ? `${machine} · moved ${path} `
          : `${machine} · moving ${path} · ${move.job.status}${progress} `;
    }
  };

  const originalShowView = showView;
  showView = function () {
    originalShowView();
    const view = projectRoute() ? 'projects' : location.hash.slice(1) || 'overview';
    serverlessSection.style.display = view === 'serverless' ? '' : 'none';
    usagePanel.style.display = view === 'billing' ? '' : 'none';
    if (view === 'overview') {
      fleet.style.display = '';
      byId('projectsSection').style.display = 'none';
      document.querySelector('.account-hero p').textContent = 'Your machines and spending, without the infrastructure maze.';
    } else if (view === 'machines') {
      document.querySelector('.account-hero p').textContent = 'All your machines, across every host.';
    } else if (view === 'billing') {
      document.querySelector('.account-hero p').textContent = 'One wallet, transparent usage. Open a project to see its own charges.';
      void renderUsage();
    } else if (view === 'serverless') {
      document.querySelector('.account-hero h1').textContent = 'Serverless';
      document.querySelector('.account-hero p').textContent = 'Functions, sites and data, organized by project.';
      for (const id of ['overviewStats','billingSection','projectsSection','fleetSection','memberSection','activitySection']) byId(id).style.display = 'none';
      void renderServerless();
    }
    advanced.open = ['access', 'activity'].includes(view);
    if (projectRoute()) {
      for (const id of ['overviewStats','billingSection','fleetSection','memberSection','activitySection','usagePanel','serverlessSection']) byId(id).style.display = 'none';
      byId('projectsSection').style.display = '';
      const active = allProjects.find(p => p.id === projectRoute());
      document.querySelector('.account-hero h1').textContent = active ? projectName(active) : 'Project';
      document.querySelector('.account-hero .eyebrow').textContent = 'Workspace / Projects / Project';
      document.querySelectorAll('[data-view]').forEach(link => link.removeAttribute('aria-current'));
      document.querySelector('.console-links [data-view="projects"]')?.setAttribute('aria-current', 'page');
    }
    renderProjects();
  };
  window.addEventListener('hashchange', () => showView());

  const billingHelp = document.createElement('div');
  billingHelp.id = 'billingHelp';
  byId('billingSection').append(billingHelp);
  const usagePanel = document.createElement('div'); usagePanel.id = 'usagePanel';
  usagePanel.innerHTML = '<div class="usage-head"><h3>Usage across all projects</h3><label>Period <select id="usageDays"><option value="1">Yesterday</option><option value="7" selected>Last 7 days</option><option value="30">Last 30 days</option></select></label></div><div id="usageMetrics" class="usage-metrics" aria-live="polite"></div><p class="usage-note">Completed UTC hours only. Charges are shown separately from your current wallet balance.</p>';
  byId('billingSection').after(usagePanel);
  async function renderUsage() {
    const metrics = byId('usageMetrics'); metrics.textContent = 'Loading usage…';
    try {
      const value = await api('usage?days=' + byId('usageDays').value);
      if (!accountSession) return;
      metrics.replaceChildren();
      const usage = value.usage || {};
      for (const [label, result, unit] of [
        ['Charged', credits(usage.debited_microcredits), 'credits'],
        ['CPU', ((usage.vcpu_ms || 0)/3600000).toFixed(2), 'vCPU-hours'],
        ['Memory', ((usage.ram_byte_ms || 0)/1073741824/3600000).toFixed(2), 'GiB-hours'],
        ['Network', (((usage.bytes_in || 0)+(usage.bytes_out || 0))/1073741824).toFixed(2), 'GiB'],
      ]) {
        const box = document.createElement('div');
        const small = document.createElement('small'); small.textContent = label;
        const strong = document.createElement('strong'); strong.textContent = result;
        const caption = document.createElement('span'); caption.textContent = unit;
        box.append(small,strong,caption); metrics.append(box);
      }
      byId('usagePanel').dataset.complete = String(value.coverage_complete);
      byId('usagePanel').querySelector('.usage-note').textContent = value.coverage_complete
        ? 'Completed UTC hours only. Charges are shown separately from your current wallet balance.'
        : 'Partial usage: one or more hosts did not report. Totals may be understated.';
    } catch (error) { metrics.textContent = 'Usage unavailable: ' + error.message; }
  }
  byId('usageDays').onchange = () => void renderUsage();
  function updateBilling() {
    const balance = Number.parseFloat(byId('balance').textContent.replace(/,/g, ''));
    billingHelp.dataset.blocked = String(Number.isFinite(balance) && balance <= 0);
    billingHelp.replaceChildren();
    const text = document.createElement('span');
    text.textContent = balance <= 0
      ? 'Your wallet has no available credits. A paused VM will keep its disk during the retention period. Ask your GAP operator to add credits before the deadline. '
      : 'This wallet pays for metered MicroVM usage across projects. Realtime has separate credits; other serverless resources have their own limits. ';
    const link = document.createElement('a');
    link.href = '#machines';
    link.textContent = 'Review projects →'; link.href = '#projects';
    billingHelp.append(text, link);
  }
  new MutationObserver(updateBilling).observe(byId('balance'), {childList: true, characterData: true, subtree: true});
  new MutationObserver(() => {
    document.body.classList.toggle('console-signed-in', !account.hidden);
    if (!account.hidden) { renderCards(); showView(); void renderUsage(); }
  }).observe(account, {attributes: true, attributeFilter: ['hidden']});
  document.body.classList.toggle('console-signed-in', !account.hidden);
  updateBilling();
  showView();
})();
