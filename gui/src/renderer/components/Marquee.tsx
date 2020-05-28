import React from 'react';
import styled from 'styled-components';
import { Scheduler } from '../../shared/scheduler';

const Text = styled.span(
  {
    position: 'relative',
    whiteSpace: 'nowrap',
  },
  (props: { overflow: number; alignRight: boolean; reset: boolean }) => ({
    left: props.alignRight ? -props.overflow + 'px' : '0',
    transition: `left linear ${props.reset ? 0 : props.overflow * 80}ms`,
  }),
);

interface IMarqueeProps {
  className?: string;
  children?: React.ReactNode;
}

interface IMarqueeState {
  alignRight: boolean;
  reset: boolean;
}

export default class Marquee extends React.Component<IMarqueeProps, IMarqueeState> {
  private textRef = React.createRef<HTMLSpanElement>();
  private scheduler = new Scheduler();

  public state = {
    alignRight: false,
    reset: false,
  };

  public componentDidMount() {
    this.startAnimationIfOverflow();
  }

  // When props.children change the content should be reset to the left and restarted if it doesn't
  // fit it should start animating.
  public componentDidUpdate(prevProps: IMarqueeProps) {
    if (this.props.children !== prevProps.children) {
      // Reset content to the left.
      this.scheduler.cancel();
      this.setState({
        alignRight: false,
        reset: true,
      });
    } else if (this.state.reset) {
      // Restart animation if it was reset.
      this.setState({ reset: false }, this.startAnimationIfOverflow);
    }
  }

  public componentWillUnmount() {
    this.scheduler.cancel();
  }

  public render() {
    return (
      <div>
        <Text
          ref={this.textRef}
          className={this.props.className}
          overflow={this.calculateOverflow()}
          alignRight={this.state.alignRight}
          reset={this.state.reset}
          onTransitionEnd={this.scheduleToggleAlignRight}>
          {this.props.children}
        </Text>
      </div>
    );
  }

  private startAnimationIfOverflow = () => {
    if (this.calculateOverflow() > 0) {
      this.scheduleToggleAlignRight();
    }
  };

  private scheduleToggleAlignRight = () => {
    this.scheduler.schedule(() => {
      this.setState((state) => ({ alignRight: !state.alignRight }));
    }, 2000);
  };

  private calculateOverflow() {
    const textWidth = this.textRef.current?.offsetWidth ?? 0;
    const parentWidth = this.textRef.current?.parentElement?.offsetWidth ?? 0;
    return textWidth - parentWidth;
  }
}
