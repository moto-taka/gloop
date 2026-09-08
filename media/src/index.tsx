import React from 'react';
import {AbsoluteFill, Composition, Easing, interpolate, registerRoot, spring, useCurrentFrame} from 'remotion';
import recording from '../public/terminal-frames.json';

const ink = '#eaf1f5', muted = '#8794a7', green = '#9ce0bd', bg = '#0b111c';
const mono = 'Menlo, "DejaVu Sans Mono", monospace';
const sans = 'Arial, sans-serif';
const ramp = (f: number, start: number, end: number) => interpolate(f, [start, end], [0, 1], {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'});
const ease = (f: number, start: number, end: number) => interpolate(f, [start, end], [0, 1], {extrapolateLeft: 'clamp', extrapolateRight: 'clamp', easing: Easing.inOut(Easing.cubic)});

function Brand() {
  return <div style={{display: 'flex', alignItems: 'center', gap: 15, fontWeight: 700, fontSize: 32, letterSpacing: -1}}>
    <svg width="36" height="32" viewBox="0 0 36 32"><path d="M6 6L27 16M6 16H27M6 26L27 16" stroke={green} strokeWidth="2"/>{[6,16,26].map(y => <circle key={y} cx="6" cy={y} r="3" fill={green}/>)}<circle cx="28" cy="16" r="5" fill={green}/></svg>
    gloop
  </div>;
}
function Backdrop() {
  return <AbsoluteFill style={{background: bg}}><AbsoluteFill style={{background: 'radial-gradient(ellipse at 50% 44%, #18302966 0%, transparent 64%)'}}/><AbsoluteFill style={{opacity: .15, backgroundImage: 'radial-gradient(#75889b 1px, transparent 1px)', backgroundSize: '32px 32px'}}/></AbsoluteFill>;
}
function Footer({right}: {right: string}) {
  return <div style={{position: 'absolute', left: 96, right: 96, bottom: 48, display: 'flex', justifyContent: 'space-between', alignItems: 'center', color: muted, fontSize: 22}}><Brand/><span>{right}</span></div>;
}

function TerminalDemo() {
  const f = useCurrentFrame();
  const t = f / 30;
  const snapshot = recording.frames.findLast(frame => frame.time <= t) ?? recording.frames[0];
  const stages = [
    {at: 0, n: '01 / OPEN', text: 'Your workflow starts in the terminal.', detail: 'Type gloop. Open a saved graph.'},
    {at: 7, n: '02 / RUN', text: 'Three checks. Running together.', detail: 'Independent commands, scheduled by gloop.'},
    {at: 21, n: '03 / COMBINE', text: 'Every result, brought together.', detail: 'The final step waits for all three checks.'},
  ];
  const stage = stages.findLast(stage => stage.at <= t)!;
  return <AbsoluteFill style={{color: ink, fontFamily: sans}}>
    <Backdrop/>
    <div style={{position: 'absolute', top: 54, left: 96, right: 96, display: 'flex', justifyContent: 'space-between', color: green, fontSize: 19, letterSpacing: 3}}><span>{stage.n}</span><span style={{color: muted}}>ACTUAL TERMINAL · LOCAL COMMAND DEMO</span></div>
    <div style={{position: 'absolute', left: 96, top: 97, fontSize: 58, fontWeight: 700, letterSpacing: -2}}>{stage.text}</div>
    <div style={{position: 'absolute', left: 96, top: 170, color: muted, fontSize: 26}}>{stage.detail}</div>
    <div style={{position: 'absolute', left: 96, top: 232, width: 1728, height: 738, background: '#131b27', border: '1px solid #344252', borderRadius: 18, overflow: 'hidden', boxShadow: '0 24px 100px #0008'}}>
      <div style={{height: 46, borderBottom: '1px solid #2a3646', display: 'flex', alignItems: 'center', paddingLeft: 24, gap: 9}}>{['#ea7777','#e7c176','#8bd4ae'].map(c => <span key={c} style={{width: 11, height: 11, background: c, borderRadius: 20}}/>)}<span style={{marginLeft: 615, color: muted, fontFamily: mono, fontSize: 15}}>gloop</span><span style={{marginLeft: 'auto', paddingRight: 24, color: green, fontSize: 13, letterSpacing: 2}}>PTY RECORDING</span></div>
      <div style={{padding: '9px 18px', fontFamily: mono, fontSize: 19.5, lineHeight: '21px', whiteSpace: 'pre', fontVariantLigatures: 'none'}}>
        {snapshot.rows.map((row, y) => <div key={y} style={{height: 21}}>{row.map((run, x) => <span key={x} style={{color: run.style.fg, background: run.style.bg, fontWeight: run.style.bold ? 700 : 400, opacity: run.style.dim ? .65 : 1}}>{run.text}</span>)}</div>)}
      </div>
    </div>
    <Footer right="Real gloop 0.8.1 · Sample checks · No model calls"/>
    <div style={{position: 'absolute', bottom: 0, height: 3, width: `${f / 929 * 100}%`, background: green}}/>
  </AbsoluteFill>;
}

const lanes = [
  {name: 'Explore', sub: 'Map the options', color: '#9ce0bd', x: 175, end: 281, tag: 'Options mapped'},
  {name: 'Compare', sub: 'Weigh the trade-offs', color: '#a7bafb', x: 740, end: 339, tag: 'Trade-offs clear'},
  {name: 'Verify', sub: 'Check the constraints', color: '#f0bb8c', x: 1305, end: 391, tag: 'Constraints checked'},
];
function Check({color = ink}: {color?: string}) {
  return <svg width="24" height="24" viewBox="0 0 24 24"><path d="M5 12l4 4L19 6" fill="none" stroke={color} strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"/></svg>;
}
function ConceptDemo() {
  const f = useCurrentFrame();
  const final = ramp(f, 478, 510);
  const title = f < 160 ? 'One task. Three independent lanes.' : f < 400 ? 'Each lane moves at its own pace.' : f < 500 ? 'Bring every result together.' : 'Parallel work. One clear result.';
  const subtitle = f < 400 ? 'Give each step its own tool, model and instructions.' : 'gloop coordinates the handoff. Your tools do the work.';
  return <AbsoluteFill style={{color: ink, fontFamily: sans}}>
    <Backdrop/>
    <div style={{position: 'absolute', top: 64, left: 96, color: green, fontSize: 19, letterSpacing: 3}}>HOW GLOOP WORKS</div>
    <div style={{position: 'absolute', top: 111, left: 96, fontSize: 74, fontWeight: 700, letterSpacing: -3}}>{title}</div>
    <div style={{position: 'absolute', top: 205, left: 99, color: muted, fontSize: 28}}>{subtitle}</div>
    <div style={{position: 'absolute', top: 291, left: 828, width: 264, height: 53, borderRadius: 30, border: '1px solid #496456', background: '#192b24', display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 12, fontSize: 22, opacity: ramp(f, 4, 20)}}><span style={{width: 8, height: 8, borderRadius: 5, background: green}}/>A shared objective</div>
    <svg width="1920" height="1080" style={{position: 'absolute', top: 0, left: 0}}>
      {lanes.map((lane, i) => {
        const cx = lane.x + 220;
        const input = `M960 344 V373 Q960 389 ${cx === 960 ? 960 : (cx < 960 ? 940 : 980)} 389 H${cx} V434`;
        const output = `M${cx} 674 V719 Q${cx} 735 ${cx + (cx < 960 ? 16 : -16)} 735 H960 V797`;
        return <g key={lane.name}>
          <path d={input} fill="none" stroke="#2d3c47" strokeWidth="2" opacity={ramp(f, 25, 55)}/>
          <path d={input} fill="none" stroke={lane.color} strokeWidth="3" pathLength="1" strokeDasharray="1" strokeDashoffset={1 - ease(f, 40 + i * 8, 85 + i * 8)} opacity={1 - ramp(f, 118, 143)}/>
          <path d={output} fill="none" stroke="#2d3c47" strokeWidth="2" opacity={ramp(f, 72, 98)}/>
          <path d={output} fill="none" stroke={lane.color} strokeWidth="3" pathLength="1" strokeDasharray="1" strokeDashoffset={1 - ease(f, 409 + i * 13, 469 + i * 13)} opacity={.85}/>
        </g>;
      })}
    </svg>
    {lanes.map((lane, i) => {
      const enter = spring({frame: f - 47 - i * 9, fps: 30, config: {damping: 22, stiffness: 100}});
      const p = ease(f, 115 + i * 6, lane.end);
      const done = f >= lane.end;
      return <div key={lane.name} style={{position: 'absolute', left: lane.x, top: 434, width: 440, height: 240, borderRadius: 22, background: '#141e2b', border: `1px solid ${done ? lane.color + 'aa' : '#384453'}`, transform: `translateY(${(1 - enter) * 32}px)`, opacity: enter, boxShadow: done ? `0 0 48px ${lane.color}0a` : '0 15px 45px #0003'}}>
        <div style={{position: 'absolute', top: 25, left: 28, fontSize: 17, letterSpacing: 2, color: lane.color}}>LANE 0{i + 1}</div>
        <div style={{position: 'absolute', top: 63, left: 28, fontSize: 43, fontWeight: 700, letterSpacing: -1}}>{lane.name}</div>
        <div style={{position: 'absolute', top: 120, left: 29, color: muted, fontSize: 24}}>{lane.sub}</div>
        <div style={{position: 'absolute', top: 177, left: 29, width: 382, height: 4, background: '#2b3545', borderRadius: 4}}><div style={{width: `${p * 100}%`, height: 4, background: lane.color, borderRadius: 4}}/></div>
        <div style={{position: 'absolute', top: 198, left: 29, color: done ? lane.color : muted, fontSize: 17}}>{done ? lane.tag : f < 115 ? 'Ready' : ['Working', 'Working.', 'Working..'][Math.floor(f / 14) % 3]}</div>
        <div style={{position: 'absolute', right: 28, top: 28, width: 35, height: 35, display: 'grid', placeItems: 'center', color: lane.color}}>{done ? <Check color={lane.color}/> : <svg width="35" height="35" style={{transform: `rotate(${f * (i + 2)}deg)`}}><circle cx="17.5" cy="17.5" r="12" stroke={lane.color} strokeWidth="2" fill="none" strokeDasharray="24 52" opacity={f >= 115 ? 1 : .3}/></svg>}</div>
      </div>;
    })}
    <div style={{position: 'absolute', left: 656, top: 797, width: 608, height: 119, borderRadius: 20, background: final > .5 ? '#1b342b' : '#111b27', border: `1px solid ${final > .5 ? green : '#384453'}`, opacity: ramp(f, 77, 104), boxShadow: `0 0 ${final * 65}px #9ce0bd15`, display: 'flex', alignItems: 'center', padding: '0 35px', boxSizing: 'border-box', gap: 24}}>
      <div style={{width: 50, height: 50, borderRadius: 14, background: final > .5 ? green : '#263342', display: 'grid', placeItems: 'center'}}>{final > .5 ? <Check color="#152c23"/> : <span style={{fontSize: 28, color: muted}}>⋈</span>}</div>
      <div><div style={{fontSize: 31, fontWeight: 700, letterSpacing: -.5}}>{final > .5 ? 'One combined result' : 'Combine the results'}</div><div style={{fontSize: 20, color: final > .5 ? green : muted, marginTop: 9}}>{final > .5 ? 'All three perspectives, ready to use.' : 'Starts when every dependency is ready.'}</div></div>
    </div>
    <Footer right="Independent tools. Deterministic orchestration."/>
  </AbsoluteFill>;
}

registerRoot(() => <><Composition id="Terminal" component={TerminalDemo} durationInFrames={930} fps={30} width={1920} height={1080}/><Composition id="Parallel" component={ConceptDemo} durationInFrames={630} fps={30} width={1920} height={1080}/></>);
