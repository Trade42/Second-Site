// Local decision prototype. Listed terms are snapshots; checks are illustrative and unresolved.
const places=[
 {id:'mateo-109',address:'915 Mateo St',suite:'109',rent:2211,sqft:1128,insight:'Mateo has the lowest listed rent. Plumbing, electrical work, and lease extras are still unpriced, so we can’t call it the cheapest to open.'},
 {id:'sixth-110b',address:'630 W 6th St',suite:'110B',rent:4048,sqft:1310,insight:'Sixth Street has a triple-net lease. Additional charges are unpriced in this comparison; compare those alongside build-out costs before choosing.'},
 {id:'beverly-720',address:'1125 S Beverly Dr',suite:'720',rent:5215,sqft:1490,insight:'Beverly has the largest listed area and highest asking rent. Full-service lease terms still need checking; neither the scan nor the rent proves it is ready for your studio.'}
];
const q=s=>document.querySelector(s);
const money=n=>'$'+n.toLocaleString('en-US');
let active=places.find(p=>p.id===new URLSearchParams(location.search).get('property'))||places[0];
let view='dollhouse';
const checks={
 plumbing:{title:'Plumbing capacity',note:'The scan doesn’t establish water capacity or hidden pipe routes.'},
 power:{title:'Equipment power requirements',note:'Available electrical capacity needs current documentation.'},
 layout:{title:'Three-room layout',note:'Room fit and clearances need a measured layout test.'}
};
function renderView(){
 const sources={dollhouse:'Matterport scan',floor:'Matterport floor plan',aerial:'Vexcel aerial',map:'OpenStreetMap contributors',photo:'CoStar listing photo'};
 const paths={dollhouse:`assets/dollhouses/${active.id}.png`,floor:`assets/${active.id}/matterport-floorplan-01.jpg`,aerial:`assets/${active.id}/vexcel-ortho.jpg`,map:'jims-assets/los-angeles-map.png',photo:`assets/${active.id}/costar-01.jpg`};
 q('#scene-image').src=paths[view];q('#scene-image').alt=`${active.address}, ${view} reference`;
 q('#view-source').textContent=sources[view];
 document.querySelectorAll('[data-view]').forEach(b=>b.setAttribute('aria-selected',String(b.dataset.view===view)));
}
function select(p){
 active=p;
 document.querySelectorAll('[data-property]').forEach(b=>b.setAttribute('aria-pressed',String(b.dataset.property===p.id)));
 document.querySelectorAll('[data-column]').forEach(c=>c.dataset.selected=String(c.dataset.column===p.id));
 q('#initial-insight').textContent=p.insight;
 const href='jims-furnish-wireframe.html?property='+p.id;
 q('#preview-layout').href=href;q('#furnish-link').href=href;
 renderView();
}
places.forEach(p=>{
 const b=document.createElement('button');b.type='button';b.className='property';b.dataset.property=p.id;b.setAttribute('aria-label',`${p.address}, suite ${p.suite}, ${money(p.rent)} asking per month, ${p.sqft} square feet`);
 const img=document.createElement('img');img.src=`assets/${p.id}/costar-01.jpg`;img.alt='';
 const overlay=document.createElement('span');overlay.className='overlay';
 const name=document.createElement('strong');name.textContent=p.address;
 const facts=document.createElement('small');facts.textContent=`${money(p.rent)}/mo | ${p.sqft.toLocaleString()} SF`;
 const suite=document.createElement('small');suite.textContent='Suite '+p.suite;
 const label=document.createElement('span');label.className='selected-label';label.textContent='Viewing';
 overlay.append(name,facts,suite);b.append(img,overlay,label);b.onclick=()=>select(p);q('.finalists').append(b);
});
for(const b of document.querySelectorAll('[data-view]'))b.onclick=()=>{view=b.dataset.view;renderView()};
for(const detail of document.querySelectorAll('[data-risk]'))detail.addEventListener('toggle',()=>{
 if(!detail.open)return;
 for(const other of document.querySelectorAll('[data-risk]'))if(other!==detail)other.open=false;
 const check=checks[detail.dataset.risk];q('#focus-title').textContent=check.title;q('#focus-note').textContent=check.note;
});
q('#focus-view').onclick=()=>{const expanded=q('.app').classList.toggle('focused');q('#focus-view').textContent=expanded?'Back to comparison':'Expand view';q('#focus-view').setAttribute('aria-pressed',String(expanded))};
function message(text,user=false){const el=document.createElement('div');el.className='message '+(user?'user':'assistant');el.textContent=text;q('.messages').append(el);q('.messages').scrollTop=q('.messages').scrollHeight}
function questions(){message('What should I ask the manager?',true);message(`For ${active.address}: which existing water and drain connections are available, what current electrical service and spare capacity are documented, and which lease charges sit outside the asking rent? These are draft questions; nothing has been sent.`)}
q('#prepare-questions').onclick=questions;
q('form').onsubmit=e=>{e.preventDefault();const input=q('#chat-input'),text=input.value.trim();if(!text)return;message(text,true);input.value='';if(/plumb|water|sink|pipe/i.test(text)){q('[data-risk=plumbing]').open=true;message('We need the water and drain connection locations, available capacity, and a contractor’s scope before pricing new plumbing. Visible fixtures alone don’t establish that.')}else if(/power|electri|panel/i.test(text)){q('[data-risk=power]').open=true;message('Ask for current service and panel information, then compare it with your equipment loads. Any upgrade cost is still unknown.')}else if(/room|fit|layout/i.test(text)){q('[data-risk=layout]').open=true;message('The next step is a measured three-room layout with equipment and circulation clearances. This prototype has not validated that fit.')}else{message(active.insight)}};
select(active);
