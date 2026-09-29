import React from 'react';
import {Composition, registerRoot} from 'remotion';
import {FreeVmMotion} from './motion';

const Root: React.FC = () => <Composition id="FreeVmMotion" component={FreeVmMotion}
  durationInFrames={900} fps={30} width={1280} height={720}/>;

registerRoot(Root);
